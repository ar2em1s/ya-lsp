//! `db/*structure.sql`, read: the other half of the schema reader, and the only reader here that is
//! not Ruby.
//!
//! An application with `config.active_record.schema_format = :sql` has no `db/schema.rb`, so
//! [`schema`](super::schema) answers **nothing** about it: a cliff, not a gradient.
//!
//! # What this is, and deliberately is not
//!
//! It is **not** a SQL parser and must not become one. A parser must understand the whole file, and
//! a real dump holds plpgsql function bodies, triggers, views, enum types and extensions, so a
//! parser that chokes on any one loses every table in the file. This is a scanner: it finds
//! `CREATE TABLE` at the top level, reads the body, and **skips everything it does not recognise**,
//! the posture of every reader in this directory.
//!
//! "Top level" is the key word, and the whole machinery: [`Scan`] knows the six things that hide
//! text from a scanner (`'…'`, `"…"`, `` `…` ``, `$tag$…$tag$`, `-- …` and `/* … */`), and nothing
//! outside those can be mistaken for a table. That makes "skips what it does not recognise" true,
//! not nearly true.
//!
//! # Three dialects, one grammar, and one word that cannot be shared
//!
//! `CREATE TABLE`, a name, parentheses, and comma-separated items are the same in all three dumps
//! Rails can produce, and so is the column rule: **a quoted identifier is always a column**,
//! because a dumper quotes exactly what its own dialect reserves. Exactly one token means opposite
//! things:
//!
//! - Postgres does not reserve `key`, so pg_dump writes `key character varying(50) NOT NULL` bare:
//!   a **column**.
//! - MySQL does reserve it, so mysqldump writes a column as `` `key` `` and an index as bare
//!   `KEY name (cols)`.
//!
//! Read it wrong one way and every column named `key` disappears; the other way, MySQL's index
//! names become members. So the dialect is detected once per file and decides exactly that one
//! thing; see [`Dialect`].
//!
//! # Where this ends
//!
//! At [`Schema`], which belongs to [`schema`](super::schema). The type table below maps a dump's
//! vocabulary onto **the word the Ruby dumper would have written**, not onto a Ruby class, so
//! `Schema::signatures`, [`rbs_type`](super::schema::rbs_type), the provenance line, the `retyped`
//! withdrawal and every consumer downstream are the `.rb` reader's. The two readers cannot disagree
//! about a database, because only one decides what a `string` returns.

use super::schema::{Column, Schema, Table, is_column_name};

/// Which dumper wrote the file, for the one question where it matters.
///
/// Detected from the contents, not the name, because the name is always `structure.sql`.
/// mysqldump's two tells are unmistakable and appear in every dump it writes: the `/*!…*/`
/// conditional-execution comment (MySQL-only syntax) and the `ENGINE=` clause after every table.
///
/// A substring test, not a scan, and the **asymmetry is why that suffices**. Either tell could in
/// principle appear in another dump's string literal, but guessing MySQL wrongly only drops columns
/// literally named `key` (failing toward nothing), while guessing standard wrongly would declare
/// MySQL index names as members (the bad direction), which cannot happen because mysqldump always
/// writes both tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// pg_dump or `sqlite3 .schema`. They differ in vocabulary, which the type table covers, and in
    /// nothing this scanner must decide.
    Standard,
    /// mysqldump, where a bare `KEY` in a table body is an index and never a column.
    MySql,
}

impl Dialect {
    fn of(source: &str) -> Self {
        if source.contains("/*!") || source.contains("ENGINE=") {
            Self::MySql
        } else {
            Self::Standard
        }
    }
}

/// Read every table `source` creates. Text in, no graph and no I/O: [`read_schema`]'s contract.
///
/// [`read_schema`]: super::read_schema
#[must_use]
pub fn read_structure(source: &str) -> Schema {
    let dialect = Dialect::of(source);
    let mut scan = Scan::new(source);
    let mut tables = Vec::new();
    while let Some(table) = scan.next_table(dialect) {
        tables.push(table);
    }
    Schema::from_tables(tables)
}

/// A body item that is a constraint or index, not a column, in any dialect.
///
/// Tested against the **unquoted** first word only, which keeps the list safely short: a dumper
/// quotes a column whose name collides with one of these, so `` `unique` `` and `"check"` are
/// columns and never reach this. Everything else in a table body is a column, including a generated
/// one, a `PRIMARY KEY` written inline after a column's type, and a type this crate has never seen.
const NOT_COLUMNS: [&str; 11] = [
    "primary",
    "unique",
    "foreign",
    "constraint",
    "index",
    "check",
    "fulltext",
    "spatial",
    "exclude",
    "period",
    "like",
];

/// What may follow a column name where a type would go, and is not one.
///
/// Only consulted for the **first** word, and only matters for a hand-edited file: every dumper
/// writes a type for every column. `character` is deliberately absent (it begins Postgres'
/// `character varying`), and `with`/`without` likewise, since neither can begin a type.
const MODIFIERS: [&str; 7] = [
    "not",
    "null",
    "default",
    "collate",
    "generated",
    "references",
    "as",
];

/// Each dumper's word for a type, in the Ruby dumper's vocabulary.
///
/// The direction is the point. Rails has no table mapping a dump's vocabulary (its
/// `NATIVE_DATABASE_TYPES` is what it *asks* a server for, and pg's read-direction map is keyed on
/// catalog names like `int4`, `float8`, `varchar`, while pg_dump writes standard spellings), so
/// this table is written here. It stops at `schema.rb`'s word instead of going on to a Ruby class,
/// which keeps the two readers from disagreeing: `character varying` becomes `string` and lands
/// wherever `t.string` lands, and if `COLUMN_TYPES` ever grows a row, both readers gain it at once.
///
/// A spelling missing from this table is **not** an error and is not dropped. It passes through
/// under its own name, lands on `untyped` just as `t.jsonb` does, and is quoted in the provenance
/// line so the card names what the file said. The unknowns in practice are exotic types like
/// pgvector's `halfvec`, Postgres enums and range types.
///
/// One row carries parentheses, deliberately. MySQL has no boolean: `t.boolean` becomes
/// `tinyint(1)` and `t.integer limit: 1` becomes `tinyint`, so the width is the only discriminator,
/// the same one ActiveRecord's `emulate_booleans` uses. So lookup tries the spelling with its width
/// first, then without.
const TYPES: [(&str, &str); 49] = [
    // Postgres, as pg_dump spells it.
    ("character varying", "string"),
    ("character", "string"),
    ("text", "text"),
    ("bytea", "binary"),
    ("smallint", "integer"),
    ("integer", "integer"),
    ("bigint", "bigint"),
    ("smallserial", "integer"),
    ("serial", "integer"),
    ("bigserial", "bigint"),
    ("real", "float"),
    ("double precision", "float"),
    ("numeric", "decimal"),
    ("boolean", "boolean"),
    ("timestamp without time zone", "datetime"),
    // What Rails dumps under the default `datetime_type`, and not a `datetime`: see `COLUMN_TYPES`.
    ("timestamp with time zone", "timestamptz"),
    ("timestamp", "datetime"),
    ("timestamptz", "timestamptz"),
    ("date", "date"),
    ("time without time zone", "time"),
    // No ActiveRecord type reads it, so the driver's `String` stands.
    ("time with time zone", "timetz"),
    ("timetz", "timetz"),
    ("time", "time"),
    ("bit varying", "bit_varying"),
    // The catalog spellings, for a hand-edited `structure.sql`, which is common in projects that
    // chose this format, so the rows are worth having.
    ("int2", "integer"),
    ("int4", "integer"),
    ("int8", "bigint"),
    ("float4", "float"),
    ("float8", "float"),
    ("bpchar", "string"),
    ("varchar", "string"),
    // MySQL, as mysqldump spells it, and SQLite, which keeps whatever Rails declared.
    ("char", "string"),
    ("tinyint(1)", "boolean"),
    ("tinyint", "integer"),
    ("mediumint", "integer"),
    ("int", "integer"),
    ("float", "float"),
    ("double", "float"),
    ("decimal", "decimal"),
    ("datetime", "datetime"),
    ("binary", "binary"),
    ("varbinary", "binary"),
    // On MySQL the width of a `t.binary` or `t.text` picks the type name, and all four of each are
    // the same Ruby `String`. Without them, `solid_cache`'s `t.binary :value` (a `longblob`) would
    // be the one column its three dumps disagree about, which the three-dialect test catches.
    ("tinyblob", "binary"),
    ("blob", "binary"),
    ("mediumblob", "binary"),
    ("longblob", "binary"),
    ("tinytext", "text"),
    ("mediumtext", "text"),
    ("longtext", "text"),
];

/// Multi-word type names, longest first.
///
/// A type is one word unless it is one of these, which is why there is a list instead of a rule
/// about where a type stops: MySQL writes `varchar(255) CHARACTER SET utf8mb4` and Postgres writes
/// `character varying`, so a rule ending a type at the word `character` would read the first right
/// and truncate the second to nothing.
const COMPOUND: [&str; 7] = [
    "timestamp without time zone",
    "timestamp with time zone",
    "time without time zone",
    "time with time zone",
    "character varying",
    "double precision",
    "bit varying",
];

/// A cursor over the file that can tell text from what merely looks like it.
struct Scan<'src> {
    source: &'src str,
    bytes: &'src [u8],
    at: usize,
}

impl<'src> Scan<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            at: 0,
        }
    }

    /// Whether `at` begins something whose contents are not SQL, and where it ends.
    ///
    /// All six are ways a scanner could read a table that is not there: a `CREATE TABLE` inside a
    /// plpgsql body, inside a comment mysqldump wrapped a statement in, or inside a default value's
    /// string literal. An unterminated one runs to the end of the file, which is what a truncated
    /// dump is, and the safe direction: a scanner that recovered would resume in the middle of a
    /// quoted region.
    ///
    /// Bytes, not `&str`, throughout, and not only for speed: four scans ask this once per byte,
    /// and `at` walks a byte at a time, so a UTF-8 continuation byte in a table's comment is
    /// ordinary input here, not a panicking slice. Every opener is ASCII, so a position that
    /// answers `Some` is always a character boundary, and callers reading text can do so directly.
    fn hidden(&self, at: usize) -> Option<usize> {
        let rest = self.bytes.get(at..).filter(|rest| !rest.is_empty())?;
        match rest[0] {
            quote @ (b'\'' | b'"' | b'`') => Some(self.quoted(at, quote)),
            b'-' if rest.starts_with(b"--") => {
                Some(memchr(rest, b'\n').map_or(self.bytes.len(), |end| at + end + 1))
            }
            b'/' if rest.starts_with(b"/*") => {
                Some(find(&rest[2..], b"*/").map_or(self.bytes.len(), |end| at + 2 + end + 2))
            }
            b'$' => dollar_tag(rest).map(|tag| {
                find(&rest[tag.len()..], tag)
                    .map_or(self.bytes.len(), |end| at + tag.len() + end + tag.len())
            }),
            _ => None,
        }
    }

    /// Where the quoted run opened at `at` ends: one past its closing quote, or the end of the file
    /// if it never closes.
    fn quoted(&self, at: usize, quote: u8) -> usize {
        let mut i = at + 1;
        while i < self.bytes.len() {
            if self.bytes[i] == quote {
                // A doubled quote is the escape all three dialects use for a quote inside its own
                // quoting.
                if self.bytes.get(i + 1) == Some(&quote) {
                    i += 2;
                    continue;
                }
                return i + 1;
            }
            i += 1;
        }
        self.bytes.len()
    }

    /// The next `CREATE [TEMPORARY|UNLOGGED|…] TABLE [IF NOT EXISTS] name (…)`, or nothing.
    fn next_table(&mut self, dialect: Dialect) -> Option<Table> {
        while self.at < self.bytes.len() {
            if let Some(end) = self.hidden(self.at) {
                self.at = end;
                continue;
            }
            // The first byte before the keyword test, because this runs once per byte of the file,
            // and `eq_ignore_ascii_case` on every byte is most of the scan.
            if !matches!(self.bytes[self.at], b'C' | b'c') {
                self.at += 1;
                continue;
            }
            let Some(after) = self.word(self.at, "create") else {
                self.at += 1;
                continue;
            };
            self.at += 1;
            let Some(header) = self.table_header(after) else {
                continue;
            };
            let Some((name, open)) = header else {
                continue;
            };
            let Some((close, items)) = self.body(open) else {
                // An unclosed body is a truncated file: there is nothing after it to find, and
                // resuming inside it would read a fragment as a table.
                self.at = self.bytes.len();
                return None;
            };
            self.at = close + 1;
            return Some(Table {
                columns: items
                    .into_iter()
                    .filter_map(|(from, to)| self.column(from, to, dialect))
                    .collect(),
                name,
                key: None,
            });
        }
        None
    }

    /// The part of a `CREATE …` that says it is a table, and its name.
    ///
    /// `None` for a `CREATE` that is not a table's (a sequence, an index, a function), and
    /// `Some(None)` for a table whose name or body this scanner will not read: the difference
    /// between "keep looking here" and "this was a table and it was skipped".
    #[allow(clippy::option_option)]
    fn table_header(&self, after_create: usize) -> Option<Option<(String, usize)>> {
        let mut at = self.skip(after_create);
        // `GLOBAL`, `LOCAL`, `TEMP`, `TEMPORARY`, `UNLOGGED`: every dumper's modifiers, in any
        // order, since none changes what the body means.
        loop {
            let Some(next) = ["global", "local", "temporary", "temp", "unlogged"]
                .into_iter()
                .find_map(|modifier| self.word(at, modifier))
            else {
                break;
            };
            at = self.skip(next);
        }
        let mut at = self.skip(self.word(at, "table")?);
        for exists in ["if", "not", "exists"] {
            let Some(next) = self.word(at, exists) else {
                break;
            };
            at = self.skip(next);
        }
        let Some((name, after)) = self.identifier(at) else {
            return Some(None);
        };
        let open = self.skip(after);
        if self.bytes.get(open) != Some(&b'(') {
            // `CREATE TABLE a PARTITION OF b`, or `… (LIKE b)` without parentheses: there is no
            // body here to read.
            return Some(None);
        }
        // A name with a line break would put a second line inside the provenance comment and take
        // the whole generated document down: `Synthesized::record` refuses RBS it cannot parse.
        // Nothing stricter is needed, since everything else is inert inside a `#` comment, and
        // nothing stricter is *wanted*: a legacy `structure.sql` is exactly where a table called
        // `OldTable` lives, and `self.table_name =` can claim it.
        if name.contains(['\n', '\r']) {
            return Some(None);
        }
        Some(Some((name, open)))
    }

    /// Whether the word at `at` is `word`, ignoring case, and where it ends.
    ///
    /// `None` at the end of the file as well as on a mismatch, for the same reason: a dump that
    /// stops mid-`CREATE` has no table there.
    fn word(&self, at: usize, word: &str) -> Option<usize> {
        let end = at + word.len();
        if !self
            .bytes
            .get(at..end)?
            .eq_ignore_ascii_case(word.as_bytes())
        {
            return None;
        }
        // A keyword only when the next byte cannot continue an identifier, or `CREATE` would match
        // the start of `CREATED_AT`.
        if self.bytes.get(end).is_some_and(|byte| is_name_byte(*byte)) {
            return None;
        }
        Some(end)
    }

    /// Past whitespace and comments: the two things that may sit between any two tokens.
    fn skip(&self, mut at: usize) -> usize {
        while let Some(byte) = self.bytes.get(at) {
            if byte.is_ascii_whitespace() {
                at += 1;
                continue;
            }
            let rest = &self.bytes[at..];
            if rest.starts_with(b"--") || rest.starts_with(b"/*") {
                at = self.hidden(at).unwrap_or(self.bytes.len());
                continue;
            }
            break;
        }
        at
    }

    /// The identifier at `at`, unquoted, with any qualification dropped.
    ///
    /// `public.stories` is the table `stories`, which is what Rails calls it; a `"quoted"` or ``
    /// `quoted` `` name is its contents. The last segment is the name; the rest says where it
    /// lives.
    fn identifier(&self, at: usize) -> Option<(String, usize)> {
        let (mut name, mut end) = self.segment(at)?;
        while self.bytes.get(end) == Some(&b'.') {
            let (next, after) = self.segment(end + 1)?;
            name = next;
            end = after;
        }
        Some((name, end))
    }

    /// One segment of an identifier: its text, and where it ends.
    fn segment(&self, at: usize) -> Option<(String, usize)> {
        match *self.bytes.get(at)? {
            quote @ (b'"' | b'`') => {
                let end = self.quoted(at, quote);
                // An unterminated quote at the very end of the file leaves nothing between the two
                // positions, the one place these indices can cross.
                let inner = self.bytes.get(at + 1..end - 1).unwrap_or_default();
                let doubled = [quote, quote];
                let doubled = String::from_utf8_lossy(&doubled).into_owned();
                Some((
                    String::from_utf8_lossy(inner)
                        .into_owned()
                        .replace(&doubled, &doubled[..1]),
                    end,
                ))
            }
            first if first.is_ascii_alphabetic() || first == b'_' => {
                let rest = &self.bytes[at..];
                let end = rest
                    .iter()
                    .position(|byte| !is_name_byte(*byte))
                    .unwrap_or(rest.len());
                // Every byte of it is ASCII by the rule above, so nothing is lost here.
                Some((String::from_utf8_lossy(&rest[..end]).into_owned(), at + end))
            }
            _ => None,
        }
    }

    /// Where the body opened at `at` closes, and the top-level ranges between its commas.
    ///
    /// One walk, because both questions need the same one: a type's own parentheses hold a comma
    /// (`numeric(10,2)`), and so does every index and constraint listing columns, so depth must be
    /// tracked either way.
    fn body(&self, at: usize) -> Option<(usize, Vec<(usize, usize)>)> {
        let mut items = Vec::new();
        let (mut start, mut i, mut depth) = (at + 1, at, 0_usize);
        while i < self.bytes.len() {
            if let Some(end) = self.hidden(i) {
                i = end;
                continue;
            }
            match self.bytes[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        items.push((start, i));
                        return Some((i, items));
                    }
                }
                b',' if depth == 1 => {
                    items.push((start, i));
                    start = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// One body item, if it is a column.
    fn column(&self, from: usize, to: usize, dialect: Dialect) -> Option<Column> {
        let start = self.skip(from);
        let quoted = matches!(self.bytes.get(start), Some(b'"' | b'`'));
        let (name, after) = self.segment(start)?;
        // `body` split this item at a comma or paren **outside** every hidden region, so a quoted
        // identifier at the item's start always closes before `to`. The clamp makes that an
        // invariant, not a promise: it costs a comparison, and the alternative is a slice that
        // panics if the split rule ever changes.
        let to = to.max(after);
        if !quoted {
            let word = name.to_ascii_lowercase();
            // The one token a dialect decides, and the only one.
            if NOT_COLUMNS.contains(&word.as_str()) || (word == "key" && dialect == Dialect::MySql)
            {
                return None;
            }
        }
        if !is_column_name(&name) {
            return None;
        }
        let (kind, array, whole) = column_type(&self.source[after..to]);
        Some(Column {
            name,
            kind,
            whole,
            nullable: !self.says_not_null(after, to),
            array,
            at: (start as u32, self.source[..to].trim_end().len() as u32),
            name_at: (
                (start + usize::from(quoted)) as u32,
                (after - usize::from(quoted)) as u32,
            ),
        })
    }

    /// Whether `NOT NULL` is written in this item, outside anything that hides text.
    ///
    /// Outside, because a default value may contain the words (`DEFAULT 'NOT NULL'` is a real
    /// possible string), and reading a column as required when it is not is exactly the wrong
    /// direction for a reader whose value is that `null: false` can be trusted.
    fn says_not_null(&self, from: usize, to: usize) -> bool {
        let mut at = from;
        while at < to {
            if let Some(end) = self.hidden(at) {
                at = end;
                continue;
            }
            if matches!(self.bytes[at], b'N' | b'n')
                && let Some(after) = self.word(at, "not")
                && self.word(self.skip(after), "null").is_some()
            {
                return true;
            }
            at += 1;
        }
        false
    }
}

/// The `$tag$` a dollar-quoted string opens with, if `rest` opens one.
///
/// Postgres' own quoting for function bodies, and why this reader must know it: the text between
/// tags is arbitrary, `CREATE TABLE` included. A `$` that opens no tag (a `$1` placeholder, a `$`
/// inside an identifier) answers `None` and is an ordinary byte.
fn dollar_tag(rest: &[u8]) -> Option<&[u8]> {
    let end = memchr(&rest[1..], b'$')? + 1;
    rest[1..end]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        .then(|| &rest[..=end])
}

/// The first `byte` in `haystack`.
fn memchr(haystack: &[u8], byte: u8) -> Option<usize> {
    haystack.iter().position(|candidate| *candidate == byte)
}

/// The first `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// What a column of this declaration returns, in the Ruby dumper's vocabulary.
///
/// The word `schema.rb` would have used, whether there are many, and whether it is a `decimal` with
/// no digits after the point ([`whole`]). An unknown spelling passes through under its own name and
/// lands on `untyped`: the same answer, reached the same way, as a `t.jsonb` in a Ruby schema.
fn column_type(rest: &str) -> (String, bool, bool) {
    // A terminator, not a filter: the first token that cannot be part of a type name ends the type,
    // and if that is the *first* token, the column has no type written at all.
    let mut words = rest.split_whitespace();
    let Some(first) = words
        .next()
        .filter(|word| is_type_word(word))
        .map(str::to_ascii_lowercase)
    else {
        return ("untyped".to_owned(), false, false);
    };
    // A qualified type belongs to an enum or extension (`public.halfvec`), and its schema is noise,
    // as a table's is.
    let mut spelling = first.rsplit('.').next().unwrap_or_default().to_owned();
    if NOT_COLUMNS.contains(&spelling.as_str()) || MODIFIERS.contains(&spelling.as_str()) {
        // A column with no type written at all, which only a hand-edited file has: SQLite accepts
        // `"a" NOT NULL`, and reading `not` as the type would put the word in the card.
        return ("untyped".to_owned(), false, false);
    }
    // One word, unless the words so far begin a compound name. The test uses the bare spelling,
    // because `timestamp(6) without time zone` has a width mid-name and `character varying[]` an
    // array suffix at the end.
    for word in words {
        let extended = format!("{spelling} {}", word.to_ascii_lowercase());
        if !COMPOUND
            .iter()
            .any(|compound| compound.starts_with(bare(&extended).as_str()))
        {
            break;
        }
        spelling = extended;
    }
    let array = spelling.ends_with("[]");
    let bare = bare(&spelling);
    let mapped = TYPES
        .iter()
        .find(|(sql, _)| *sql == spelling.trim_end_matches("[]"))
        .or_else(|| TYPES.iter().find(|(sql, _)| *sql == bare));
    let kind = mapped.map_or(bare, |(_, ruby)| (*ruby).to_owned());
    let whole = kind == "decimal" && whole(&spelling);
    (kind, array, whole)
}

/// Whether a `numeric`/`decimal` spelling has a precision and no digits after the point:
/// `numeric(10)` or `decimal(10,0)`, which ActiveRecord's `extract_scale` answers 0 for, so it
/// registers `DecimalWithoutScale`, an `Integer`. A bare `numeric` has no precision and is a
/// `BigDecimal`.
fn whole(spelling: &str) -> bool {
    let Some(width) = spelling
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(width, _)| width)
    else {
        return false;
    };
    // The scale is the second number, and a width with none has no digits after the point.
    width
        .split(',')
        .nth(1)
        .is_none_or(|scale| scale.trim() == "0")
}

/// Whether `word` can be a type name at all.
///
/// Asked of the **first** token only, and it exists for a real MySQL type, not as a defence:
/// `enum('draft','live')` and `set('a','b')` carry string literals, so they are declined and the
/// column is `untyped`, which is right, since neither is one of `COLUMN_TYPES` and
/// ActiveRecord does not treat them as `t.string`. Later tokens are decided by the stricter
/// compound table: no compound type name contains anything but bare words.
fn is_type_word(word: &str) -> bool {
    word.chars()
        .all(|character| character.is_ascii_alphanumeric() || "_(),.[]".contains(character))
}

/// A spelling with its widths and its array suffix taken off: what the table is keyed on.
fn bare(spelling: &str) -> String {
    let mut bare = String::with_capacity(spelling.len());
    let mut depth = 0_usize;
    for character in spelling.chars() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => bare.push(character),
            _ => {}
        }
    }
    bare.trim_end_matches("[]").trim_end().to_owned()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use crate::analysis::testing::*;
    use crate::generated::declaring;
    use std::collections::BTreeMap;

    use super::*;

    /// `solid_cache`'s three-table schema, as **pg_dump** wrote it.
    ///
    /// The gem ships all three dumps, a ready-made controlled experiment: one migration, three
    /// dumpers, and only one type name in six spelled the same by all three. Whole real files, not
    /// extracts, because half of this reader's job is *skipping*: the `SET`s, the `--` comment
    /// blocks, `CREATE SEQUENCE`, `ALTER TABLE ... ADD CONSTRAINT`, `CREATE INDEX`, and in the
    /// MySQL one the `/*!…*/` wrappers and `DROP TABLE IF EXISTS`.
    const POSTGRES: &str = r##"SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: ar_internal_metadata; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.ar_internal_metadata (
    key character varying NOT NULL,
    value character varying,
    created_at timestamp(6) without time zone NOT NULL,
    updated_at timestamp(6) without time zone NOT NULL
);


--
-- Name: schema_migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.schema_migrations (
    version character varying NOT NULL
);


--
-- Name: solid_cache_entries; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.solid_cache_entries (
    id bigint NOT NULL,
    key bytea NOT NULL,
    value bytea NOT NULL,
    created_at timestamp(6) without time zone NOT NULL,
    key_hash bigint NOT NULL,
    byte_size integer NOT NULL
);


--
-- Name: solid_cache_entries_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.solid_cache_entries_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: solid_cache_entries_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.solid_cache_entries_id_seq OWNED BY public.solid_cache_entries.id;


--
-- Name: solid_cache_entries id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.solid_cache_entries ALTER COLUMN id SET DEFAULT nextval('public.solid_cache_entries_id_seq'::regclass);


--
-- Name: ar_internal_metadata ar_internal_metadata_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.ar_internal_metadata
    ADD CONSTRAINT ar_internal_metadata_pkey PRIMARY KEY (key);


--
-- Name: schema_migrations schema_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.schema_migrations
    ADD CONSTRAINT schema_migrations_pkey PRIMARY KEY (version);


--
-- Name: solid_cache_entries solid_cache_entries_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.solid_cache_entries
    ADD CONSTRAINT solid_cache_entries_pkey PRIMARY KEY (id);


--
-- Name: index_solid_cache_entries_on_byte_size; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_solid_cache_entries_on_byte_size ON public.solid_cache_entries USING btree (byte_size);


--
-- Name: index_solid_cache_entries_on_key_hash; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX index_solid_cache_entries_on_key_hash ON public.solid_cache_entries USING btree (key_hash);


--
-- Name: index_solid_cache_entries_on_key_hash_and_byte_size; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX index_solid_cache_entries_on_key_hash_and_byte_size ON public.solid_cache_entries USING btree (key_hash, byte_size);


--
-- PostgreSQL database dump complete
--

SET search_path TO "$user", public;

"##;

    /// The same schema, as **mysqldump** wrote it.
    ///
    /// Every unshared rule shows in this one file: `` `key` `` is a column while a bare `KEY … (…)`
    /// two lines below is an index, `PRIMARY KEY` and `UNIQUE KEY` are non-column items,
    /// `tinyint`/`varbinary`/`longblob` are its own vocabulary, and `/*!` and `ENGINE=` identify
    /// the dialect.
    const MYSQL: &str = r##"
/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40101 SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS */;
/*!40101 SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION */;
/*!50503 SET NAMES utf8mb4 */;
/*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;
/*!40103 SET TIME_ZONE='+00:00' */;
/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */;
/*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */;
/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */;
/*!40111 SET @OLD_SQL_NOTES=@@SQL_NOTES, SQL_NOTES=0 */;
DROP TABLE IF EXISTS `ar_internal_metadata`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `ar_internal_metadata` (
  `key` varchar(255) NOT NULL,
  `value` varchar(255) DEFAULT NULL,
  `created_at` datetime(6) NOT NULL,
  `updated_at` datetime(6) NOT NULL,
  PRIMARY KEY (`key`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `schema_migrations`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `schema_migrations` (
  `version` varchar(255) NOT NULL,
  PRIMARY KEY (`version`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `solid_cache_entries`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `solid_cache_entries` (
  `id` bigint NOT NULL AUTO_INCREMENT,
  `key` varbinary(1024) NOT NULL,
  `value` longblob NOT NULL,
  `created_at` datetime(6) NOT NULL,
  `key_hash` bigint NOT NULL,
  `byte_size` int NOT NULL,
  PRIMARY KEY (`id`),
  UNIQUE KEY `index_solid_cache_entries_on_key_hash` (`key_hash`),
  KEY `index_solid_cache_entries_on_byte_size` (`byte_size`),
  KEY `index_solid_cache_entries_on_key_hash_and_byte_size` (`key_hash`,`byte_size`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
/*!40103 SET TIME_ZONE=@OLD_TIME_ZONE */;

/*!40101 SET SQL_MODE=@OLD_SQL_MODE */;
/*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */;
/*!40014 SET UNIQUE_CHECKS=@OLD_UNIQUE_CHECKS */;
/*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;
/*!40101 SET CHARACTER_SET_RESULTS=@OLD_CHARACTER_SET_RESULTS */;
/*!40101 SET COLLATION_CONNECTION=@OLD_COLLATION_CONNECTION */;
/*!40111 SET SQL_NOTES=@OLD_SQL_NOTES */;

"##;

    /// The same schema, as **`sqlite3 .schema`** wrote it.
    ///
    /// Four lines, every identifier quoted, and no `NOT NULL` on the column SQLite made the primary
    /// key by writing `PRIMARY KEY AUTOINCREMENT` after the type: the shape that makes
    /// `says_not_null` a scan of the item, not a look at its last two words.
    const SQLITE: &str = r##"CREATE TABLE IF NOT EXISTS "schema_migrations" ("version" varchar NOT NULL PRIMARY KEY);
CREATE TABLE IF NOT EXISTS "ar_internal_metadata" ("key" varchar NOT NULL PRIMARY KEY, "value" varchar, "created_at" datetime(6) NOT NULL, "updated_at" datetime(6) NOT NULL);
CREATE TABLE IF NOT EXISTS "solid_cache_entries" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "key" blob(1024) NOT NULL, "value" blob(536870912) NOT NULL, "created_at" datetime(6) NOT NULL, "key_hash" integer(8) NOT NULL, "byte_size" integer(4) NOT NULL);
CREATE INDEX "index_solid_cache_entries_on_byte_size" ON "solid_cache_entries" ("byte_size");
CREATE INDEX "index_solid_cache_entries_on_key_hash_and_byte_size" ON "solid_cache_entries" ("key_hash", "byte_size");
CREATE UNIQUE INDEX "index_solid_cache_entries_on_key_hash" ON "solid_cache_entries" ("key_hash");
"##;
    /// Every declaration a dump makes, as sorted `Class#def …` lines.
    ///
    /// The provenance comment is dropped on purpose: it names the file and the *schema's own word*
    /// for the type, the two things three dumps of one database may disagree about. What must not
    /// differ is the member and what it returns.
    fn declarations(source: &str) -> Vec<String> {
        let schema = read_structure(source);
        let classes: BTreeMap<String, Vec<String>> = schema
            .table_names()
            .map(|table| (table.to_owned(), vec![table.to_owned()]))
            .collect();
        let rbs = schema
            .signatures("db/structure.sql", &classes, &BTreeMap::new(), true)
            .render(&declaring(&[]))
            .rbs;
        let mut owner = String::new();
        let mut lines = Vec::new();
        for line in rbs.lines() {
            if let Some(name) = line.strip_prefix("class ") {
                owner = name.to_owned();
            } else if let Some(declaration) = line.trim().strip_prefix("def ") {
                lines.push(format!("{owner}#{declaration}"));
            }
        }
        lines.sort();
        lines
    }

    /// The schema's own word for each column, which is the half that may differ.
    fn kinds(source: &str) -> Vec<String> {
        let schema = read_structure(source);
        let classes: BTreeMap<String, Vec<String>> = schema
            .table_names()
            .map(|table| (table.to_owned(), vec![table.to_owned()]))
            .collect();
        let mut kinds: Vec<String> = schema
            .signatures("db/structure.sql", &classes, &BTreeMap::new(), true)
            .render(&declaring(&[]))
            .rbs
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("# From ")?;
                let table = rest.split("table `").nth(1)?.split('`').next()?;
                let column = rest.split("column `").nth(1)?.split('`').next()?;
                let kind = rest.rsplit("(`").next()?.split("`,").next()?;
                Some(format!("{table}.{column} {kind}"))
            })
            .collect();
        kinds.sort();
        kinds
    }

    /// The strongest property this reader has, as a test.
    ///
    /// One migration dumped by three databases must answer with **the same members returning the
    /// same types**, or "one reader, three dialects" is a claim, not a property. It holds exactly,
    /// including the two non-trivial parts: `t.binary` is `bytea`, `varbinary(1024)`/`longblob` and
    /// `blob(1024)`/`blob(536870912)` across the three files, and `t.datetime` is
    /// `timestamp(6) without time zone` in one and `datetime(6)` in the others.
    #[test]
    fn one_schema_dumped_by_three_databases_declares_one_thing() {
        let postgres = declarations(POSTGRES);
        assert_eq!(postgres, declarations(MYSQL));
        assert_eq!(postgres, declarations(SQLITE));
        assert_eq!(
            postgres,
            vec![
                "ar_internal_metadata#created_at: () -> ActiveSupport::TimeWithZone",
                "ar_internal_metadata#key: () -> String",
                "ar_internal_metadata#updated_at: () -> ActiveSupport::TimeWithZone",
                // The one nullable column in the file, and all three dumps say so the same way.
                "ar_internal_metadata#value: () -> String?",
                "schema_migrations#version: () -> String",
                "solid_cache_entries#byte_size: () -> Integer",
                "solid_cache_entries#created_at: () -> ActiveSupport::TimeWithZone",
                "solid_cache_entries#id: () -> Integer",
                // `t.binary`, which is `bytea`, `varbinary(1024)` and `blob(1024)`.
                "solid_cache_entries#key: () -> String",
                "solid_cache_entries#key_hash: () -> Integer",
                // `t.binary` again, and the widths are the point: `longblob` and `blob(536870912)`
                // are the same `String` as the 1024-byte one.
                "solid_cache_entries#value: () -> String",
            ]
        );
    }

    /// Where the three dumps *do* differ, stated instead of left to be discovered.
    ///
    /// The provenance line quotes the schema's own word, and SQLite has one integer type, so a
    /// `bigint` in the other two dumps is an `integer` there, and no reader can recover the
    /// distinction because the database never recorded it. It costs nothing: both are `Integer` in
    /// Ruby, as the test above pins. Postgres and MySQL agree on all nine.
    #[test]
    fn the_one_thing_three_dumps_of_one_database_cannot_agree_on() {
        assert_eq!(kinds(POSTGRES), kinds(MYSQL));
        let differences: Vec<(String, String)> = kinds(POSTGRES)
            .into_iter()
            .zip(kinds(SQLITE))
            .filter(|(postgres, sqlite)| postgres != sqlite)
            .collect();
        assert_eq!(
            differences,
            vec![
                (
                    "solid_cache_entries.id bigint".to_owned(),
                    "solid_cache_entries.id integer".to_owned()
                ),
                (
                    "solid_cache_entries.key_hash bigint".to_owned(),
                    "solid_cache_entries.key_hash integer".to_owned()
                ),
            ]
        );
    }

    /// The one token the dialect decides, in the file that shows both readings.
    ///
    /// mysqldump's `solid_cache_entries` has a `` `key` `` column and, five lines below, three bare
    /// `KEY …` index clauses. Read `KEY` as a column and the table gains three members named after
    /// indexes; read `` `key` `` as a keyword and real Postgres tables lose a column. Only the
    /// dialect flag separates them.
    #[test]
    fn key_is_a_column_in_two_dialects_and_an_index_in_the_third() {
        for dump in [POSTGRES, SQLITE, MYSQL] {
            let members = declarations(dump);
            assert!(
                members.contains(&"solid_cache_entries#key: () -> String".to_owned()),
                "{members:?}"
            );
            assert!(
                !members
                    .iter()
                    .any(|member| member.contains("index_solid_cache_entries")),
                "an index name is not a member: {members:?}"
            );
        }
        // And the flag really is read from the file: the same body under the other dialect reads
        // the bare `key` as the column Postgres would mean. Lowercase because pg_dump writes it
        // that way (an unquoted identifier is already canonical in a dump); one written `KEY` is
        // declined by the same text rule the Ruby schema applies to column names, which fails
        // toward nothing.
        let mysql = "CREATE TABLE `t` (\n  `a` int NOT NULL,\n  KEY `i` (`a`)\n) ENGINE=InnoDB;";
        let standard = "CREATE TABLE t (\n  a int NOT NULL,\n  key character varying(50)\n);";
        assert_eq!(columns_of(mysql, "t"), vec!["a"]);
        assert_eq!(columns_of(standard, "t"), vec!["a", "key"]);
        assert_eq!(
            columns_of(
                "CREATE TABLE t (\n  a int,\n  KEY character varying\n);",
                "t"
            ),
            vec!["a"]
        );
    }

    /// The columns one table declares, by name, in the order the file declares them.
    fn columns_of(source: &str, table: &str) -> Vec<String> {
        let schema = read_structure(source);
        let classes = [(table.to_owned(), vec![table.to_owned()])]
            .into_iter()
            .collect();
        schema
            .signatures("db/structure.sql", &classes, &BTreeMap::new(), true)
            .render(&declaring(&[]))
            .rbs
            .lines()
            .filter_map(|line| line.trim().strip_prefix("def "))
            .filter_map(|line| line.split(':').next())
            .map(str::to_owned)
            .collect()
    }

    /// A quoted identifier is a column whatever its name, and that rule needs no dialect.
    ///
    /// A dumper quotes exactly what its own dialect reserves, which is why this is the rule, not a
    /// list of reserved words to keep in sync across three databases. Real dumps quote body
    /// identifiers often enough that uppercasing a name before noticing its quotes would lose real
    /// columns.
    #[test]
    fn a_quoted_reserved_word_is_a_column() {
        let source = r#"CREATE TABLE public.orders (
    id bigint NOT NULL,
    "order" integer,
    "position" integer,
    "group" character varying,
    "check" boolean,
    "primary" boolean,
    CONSTRAINT orders_check CHECK ((id > 0)),
    PRIMARY KEY (id)
);"#;
        assert_eq!(
            columns_of(source, "orders"),
            vec!["id", "order", "position", "group", "check", "primary"]
        );
    }

    /// Everything in a dump that is not a table, in one file, around tables that must survive.
    ///
    /// This test stands in for a real DDL parser. A parser must understand all of this; a scanner
    /// must *skip* it, and the danger is real: a plpgsql body is dollar-quoted text that can hold
    /// anything, including the words this scanner looks for.
    #[test]
    fn what_is_not_a_table_and_does_not_disturb_the_ones_around_it() {
        let source = r#"SET statement_timeout = 0;

CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA public;

CREATE TYPE public.status AS ENUM ('draft', 'live');

CREATE TABLE public.before (
    id bigint NOT NULL
);

CREATE FUNCTION public.mischief() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
  -- CREATE TABLE public.never (id bigint);
  RAISE NOTICE 'CREATE TABLE public.also_never (id bigint)';
  RETURN NEW;
END;
$$;

CREATE VIEW public.a_view AS SELECT id FROM public.before;

CREATE SEQUENCE public.before_id_seq START WITH 1;

ALTER TABLE ONLY public.before ALTER COLUMN id SET DEFAULT nextval('public.before_id_seq');

CREATE TRIGGER a_trigger BEFORE INSERT ON public.before FOR EACH ROW EXECUTE FUNCTION public.mischief();

CREATE INDEX index_before_on_id ON public.before USING btree (id);

/* CREATE TABLE public.commented_out (id bigint); */

CREATE TABLE public.after (
    id bigint NOT NULL
);"#;
        assert_eq!(
            read_structure(source).table_names().collect::<Vec<_>>(),
            vec!["before", "after"]
        );
    }

    /// A body this scanner will not read, each for its own reason.
    #[test]
    fn what_is_a_table_and_declares_nothing_anyway() {
        // A `CREATE TABLE` with no body, one whose body never closes, a partition borrowing its
        // columns, and a name containing a line break, which would put a second line in the
        // provenance comment and take the whole generated document down.
        for source in [
            "CREATE TABLE public.nothing;",
            "CREATE TABLE public.unclosed (\n    id bigint NOT NULL",
            "CREATE TABLE public.part PARTITION OF public.whole FOR VALUES IN (1);",
            "CREATE TABLE \"two\nlines\" (id bigint);",
            "CREATE TABLE (id bigint);",
            "CREATED_AT TABLE public.t (id bigint);",
            "CREATE",
        ] {
            assert!(
                read_structure(source).table_names().next().is_none(),
                "{source}"
            );
        }
        // A truncated body ends the scan instead of resuming inside it: nothing follows in a file
        // that stopped mid-statement, and resuming would read a fragment.
        assert!(
            read_structure("CREATE TABLE a (id bigint,\nCREATE TABLE b (id bigint);")
                .table_names()
                .next()
                .is_none()
        );
    }

    /// What a body item has to be to become a member, and what each refusal costs.
    #[test]
    fn what_is_not_a_column() {
        let source = r#"CREATE TABLE public.oddities (
    id bigint NOT NULL,
    "MixedCase" integer,
    ok integer,
    PRIMARY KEY (id),
    UNIQUE (ok),
    FOREIGN KEY (id) REFERENCES public.other(id),
    CONSTRAINT c CHECK ((ok > 0)),
    EXCLUDE USING gist (id WITH =),
    LIKE public.other,
    ,
    9
);"#;
        // `id` and `ok` survive. Refused: a name that cannot be written into RBS (the same text
        // rule the Ruby schema applies, for the same reason), every constraint spelling of all
        // three dialects, an empty item, and one that does not begin with an identifier.
        assert_eq!(columns_of(source, "oddities"), vec!["id", "ok"]);
    }

    /// Every spelling in the table, and what it returns. A wrong row is a failing line here.
    ///
    /// The direction keeps this reviewable: each row maps a *dump's* word onto the word the Ruby
    /// dumper would write, and `COLUMN_TYPES` (one list, shared with `db/schema.rb`) decides what
    /// that returns. So the two readers cannot disagree about a database, and an unknown type
    /// passes through under its own name and lands on `untyped`, exactly like `t.jsonb`.
    #[test]
    fn every_spelling_three_dumpers_use_and_what_it_returns() {
        let rows: Vec<(&str, String)> = [
            // Postgres.
            "character varying(255)",
            "character(2)",
            "text",
            "bytea",
            "smallint",
            "integer",
            "bigint",
            "bigserial",
            "double precision",
            "numeric(10,2)",
            "numeric(10)",
            "numeric(10,0)",
            "numeric",
            "boolean",
            "timestamp(6) without time zone",
            "timestamp with time zone",
            "timestamptz",
            "date",
            "time without time zone",
            "integer[]",
            "character varying[]",
            "jsonb",
            "public.some_enum",
            "uuid",
            "inet",
            "citext",
            "bit varying(8)",
            // MySQL.
            "varchar(255)",
            "int",
            "tinyint(1)",
            "tinyint",
            "datetime(6)",
            "longblob",
            "varbinary(1024)",
            "longtext",
            "double",
            "decimal(10,2)",
            "decimal(10,0)",
            "timestamp",
            // SQLite.
            "varchar",
            "integer(8)",
            "blob(536870912)",
            "float",
            // Everything that is not a type at all.
            "",
            "NOT NULL",
            "DEFAULT ''::character varying NOT NULL",
        ]
        .into_iter()
        .map(|declaration| {
            let (kind, array, whole) = column_type(declaration);
            let kind = if whole {
                super::super::WHOLE_DECIMAL.to_owned()
            } else {
                kind
            };
            (
                declaration,
                super::super::schema::rbs_type(&kind, true, false, array),
            )
        })
        .collect();

        let expected: Vec<(&str, &str)> = vec![
            ("character varying(255)", "String"),
            ("character(2)", "String"),
            ("text", "String"),
            ("bytea", "String"),
            ("smallint", "Integer"),
            ("integer", "Integer"),
            ("bigint", "Integer"),
            ("bigserial", "Integer"),
            ("double precision", "Float"),
            ("numeric(10,2)", "BigDecimal"),
            // No digits after the point: `DecimalWithoutScale`, an `Integer`. A bare `numeric` has
            // no precision at all, and stays a `BigDecimal`.
            ("numeric(10)", "Integer"),
            ("numeric(10,0)", "Integer"),
            ("numeric", "BigDecimal"),
            ("boolean", "bool"),
            (
                "timestamp(6) without time zone",
                "ActiveSupport::TimeWithZone",
            ),
            // Time-zone aware only from Rails 7.1 on, so the class rests on a version this cannot
            // see.
            (
                "timestamp with time zone",
                "ActiveSupport::TimeWithZone | Time",
            ),
            ("timestamptz", "ActiveSupport::TimeWithZone | Time"),
            ("date", "Date"),
            // `t.time` is not one of `COLUMN_TYPES` in a Ruby schema either, and this answers
            // exactly what that does, without being cleverer.
            (
                "time without time zone",
                "ActiveSupport::TimeWithZone | Time",
            ),
            ("integer[]", "Array[Integer]"),
            ("character varying[]", "Array[String]"),
            ("jsonb", "untyped"),
            // A Postgres enum or extension type: the schema qualification is noise, the name is
            // quoted in the card, and the answer is no claim.
            ("public.some_enum", "untyped"),
            // Postgres' own types pass through under their own names, which are the words the Ruby
            // dumper writes, except the one compound.
            ("uuid", "String"),
            ("inet", "IPAddr"),
            ("citext", "String"),
            ("bit varying(8)", "String"),
            ("varchar(255)", "String"),
            ("int", "Integer"),
            // The one row that carries its width, and the reason: MySQL has no boolean.
            ("tinyint(1)", "bool"),
            ("tinyint", "Integer"),
            ("datetime(6)", "ActiveSupport::TimeWithZone"),
            ("longblob", "String"),
            ("varbinary(1024)", "String"),
            ("longtext", "String"),
            ("double", "Float"),
            ("decimal(10,2)", "BigDecimal"),
            ("decimal(10,0)", "Integer"),
            // MySQL's `timestamp` is ActiveRecord's `:datetime` there, and converted.
            ("timestamp", "ActiveSupport::TimeWithZone"),
            ("varchar", "String"),
            ("integer(8)", "Integer"),
            ("blob(536870912)", "String"),
            ("float", "Float"),
            ("", "untyped"),
            ("NOT NULL", "untyped"),
            // A shape real Postgres dumps use, and the one that made `character varying` look as
            // though it never ended.
            ("DEFAULT ''::character varying NOT NULL", "untyped"),
        ];

        assert_eq!(
            rows,
            expected
                .into_iter()
                .map(|(declaration, returns)| (declaration, returns.to_owned()))
                .collect::<Vec<_>>()
        );
    }

    /// `NOT NULL` is a scan of the item, not a look at its end, and not a substring search.
    #[test]
    fn which_columns_may_be_nil() {
        let source = r#"CREATE TABLE public.t (
    a integer NOT NULL,
    b integer,
    c integer DEFAULT 0 NOT NULL,
    d character varying DEFAULT 'NOT NULL'::character varying,
    e integer PRIMARY KEY AUTOINCREMENT NOT NULL,
    f integer NOT   NULL,
    g integer NULL
);"#;
        let schema = read_structure(source);
        let classes = [("t".to_owned(), vec!["T".to_owned()])]
            .into_iter()
            .collect();
        let signatures = schema
            .signatures("db/structure.sql", &classes, &BTreeMap::new(), true)
            .render(&declaring(&[]));
        let optional: Vec<&str> = signatures
            .rbs
            .lines()
            .filter_map(|line| line.trim().strip_prefix("def "))
            .filter(|line| line.ends_with('?'))
            .filter_map(|line| line.split(':').next())
            .collect();
        // `d`'s default is the *string* `NOT NULL`, which a substring search would read as a
        // constraint: the wrong direction, because a column wrongly read as required is a `String`
        // where the truth is `String?`.
        assert_eq!(optional, vec!["b", "d", "g"]);
    }

    /// The provenance line carries what the schema said, including that there are many of them.
    #[test]
    fn where_a_dumped_column_says_it_came_from() {
        let source = "CREATE TABLE public.stories (\n    id bigint NOT NULL,\n    tags character varying[]\n);";
        let classes = [("stories".to_owned(), vec!["Story".to_owned()])]
            .into_iter()
            .collect();
        let signatures = read_structure(source)
            .signatures("db/structure.sql", &classes, &BTreeMap::new(), true)
            .render(&declaring(&[]));
        assert_eq!(
            signatures.rbs,
            "class Story\n  \
             # From `db/structure.sql`, table `stories`, column `id` (`bigint`, `null: false`).\n  \
             def id: () -> Integer\n  \
             # From `db/structure.sql`, table `stories`, column `tags` (`string[]`, may be `nil`).\n  \
             def tags: () -> Array[String]?\n\
             end\n"
        );
        // The spans point back into the SQL, which is what a jump and a reveal need: the whole
        // item, and the identifier inside its quotes.
        let rows: Vec<(&str, &str)> = signatures
            .spans
            .iter()
            .map(|span| {
                (
                    &source[span.declared.0 as usize..span.declared.1 as usize],
                    &source[span.selection.0 as usize..span.selection.1 as usize],
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("id bigint NOT NULL", "id"),
                ("tags character varying[]", "tags"),
            ]
        );
    }

    /// The dialect is read from the file, and it is the only non-table thing read from the file.
    ///
    /// Both tells, each on its own, plus the negative case the substring test relies on: mysqldump
    /// always writes both, so a real dump cannot reach the reading that declares an index name as a
    /// member, and the reading that costs a column named `key` fails toward nothing.
    #[test]
    fn which_dumper_wrote_the_file() {
        assert_eq!(Dialect::of(MYSQL), Dialect::MySql);
        assert_eq!(Dialect::of(POSTGRES), Dialect::Standard);
        assert_eq!(Dialect::of(SQLITE), Dialect::Standard);
        assert_eq!(
            Dialect::of("CREATE TABLE `t` (`a` int) ENGINE=InnoDB;"),
            Dialect::MySql
        );
        assert_eq!(
            Dialect::of("/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE */;"),
            Dialect::MySql
        );
    }

    /// Every kind of byte in a dump a scanner must decide about, in one table.
    ///
    /// Each row is a shape from a real file that would be misread with one rule fewer: a bare `-`
    /// or `/` opening no comment, a doubled quote, a comment where whitespace would do, `UNLOGGED`,
    /// an identifier starting with `_` or containing `$`, a `NOT` that is not `NOT NULL`, and a
    /// column SQLite lets you declare with no type.
    #[test]
    fn the_bytes_a_scanner_has_to_decide_about() {
        let source = r#"CREATE UNLOGGED TABLE /* not a name */ public.oddities ( -- nor this
    _internal integer DEFAULT -1 NOT NULL,
    a$b integer,
    c$d integer,
    ratio integer CHECK ((ratio / 2) > 0),
    flag boolean CHECK (flag IS NOT TRUE),
    quoted character varying DEFAULT 'it''s' NOT NULL,
    untypedish PRIMARY KEY,
    kind enum('draft','live') NOT NULL
);"#;
        assert_eq!(
            read_structure(source).table_names().collect::<Vec<_>>(),
            vec!["oddities"]
        );
        let schema = read_structure(source);
        let classes = [("oddities".to_owned(), vec!["Oddity".to_owned()])]
            .into_iter()
            .collect();
        assert_eq!(
            schema
                .signatures("db/structure.sql", &classes, &BTreeMap::new(), true)
                .render(&declaring(&[]))
                .rbs
                .lines()
                .filter_map(|line| line.trim().strip_prefix("def "))
                .collect::<Vec<_>>(),
            vec![
                "_internal: () -> Integer",
                // `a$b` and `c$d` are **not** here: `$` is part of an identifier in SQL and opens
                // no dollar-quote (the other half of the rule function bodies rely on), but
                // `def a$b:` is not RBS, so the Ruby schema's text rule declines it. There are two,
                // so the *tag* test runs: with one `$` there is no second to look for, and with
                // two, the text between them must be rejected on its contents.
                "ratio: () -> Integer?",
                // `NOT TRUE` is not `NOT NULL`, and reading it as one would claim a nullable column
                // cannot be `nil`: the wrong direction for a nullability claim.
                "flag: () -> bool?",
                "quoted: () -> String",
                // SQLite takes a column with no type written at all; `PRIMARY KEY` is not one.
                "untypedish: () -> untyped",
                // MySQL's inline `enum`, which carries string literals and is declined: it is not
                // one of `COLUMN_TYPES`, and Rails does not treat it as `t.string`.
                "kind: () -> untyped",
            ]
        );
    }

    /// A dollar-quoted body with a named tag, and a `$` that opens nothing.
    ///
    /// pg_dump writes `$$`, which the skipping test uses; a hand-kept `structure.sql` writes
    /// `$body$`, the spelling where the tag's *contents* matter.
    #[test]
    fn a_named_dollar_quote_hides_a_table_exactly_as_an_anonymous_one_does() {
        let source = "\
CREATE FUNCTION public.f() RETURNS trigger LANGUAGE plpgsql AS $body$
BEGIN
  -- CREATE TABLE public.never (id bigint);
  RETURN NEW;
END;
$body$;

CREATE TABLE public.after (id bigint NOT NULL);
";
        assert_eq!(
            read_structure(source).table_names().collect::<Vec<_>>(),
            vec!["after"]
        );
        // A `$` with nothing to close it is an ordinary byte, and the scan carries on past it.
        assert_eq!(
            read_structure("SELECT $1;\nCREATE TABLE public.t (id bigint);")
                .table_names()
                .collect::<Vec<_>>(),
            vec!["t"]
        );
    }

    /// A quote that never closes ends the scan, and a doubled one does not.
    #[test]
    fn a_quote_that_never_closes_and_one_that_only_looks_like_it() {
        // Unterminated: the body never closes either, so the table is dropped instead of read as a
        // fragment, and nothing follows in a file that stopped mid-statement.
        assert!(
            read_structure("CREATE TABLE public.t (a integer, \"b integer);")
                .table_names()
                .next()
                .is_none()
        );
        // Doubled is the escape, so this is one identifier and not two.
        assert_eq!(
            read_structure("CREATE TABLE public.\"a\"\"b\" (id bigint);")
                .table_names()
                .collect::<Vec<_>>(),
            vec!["a\"b"]
        );
    }

    /// A file with no `CREATE TABLE` in it costs one scan and answers nothing.
    #[test]
    fn a_dump_that_creates_no_tables() {
        assert!(read_structure("").table_names().next().is_none());
        assert!(
            read_structure("SET statement_timeout = 0;\n")
                .table_names()
                .next()
                .is_none()
        );
    }

    #[test]
    fn a_dumped_column_is_a_method_that_types_its_chain_and_jumps_to_the_sql() {
        // The SQL reader's whole point: the `.rb` schema's test, against the other file. An
        // application with `schema_format = :sql` would have **no** column types without it (a
        // cliff, not a gradient), and the same three things must happen at once: the member exists,
        // the chain off it is typed, and the jump lands on the SQL line that said so.
        let source = "Story.new.title.upcase\n";
        let (mut harness, dump, uri) = sql_project(source);

        assert!(harness.has("Story#title()"), "the column is not a member");

        let chained = card(&mut harness, &uri, source, "upcase");
        assert!(chained.contains("String#upcase"), "{chained}");

        let definition = harness.definition_at(&uri, source, "title");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(dump.as_str()),
            "{definition}"
        );
        // `    title character varying NOT NULL` on line 4, revealed whole, name selected.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (
                &serde_json::json!(4),
                &serde_json::json!(4),
                &serde_json::json!(4),
            ),
            "{definition}"
        );

        // And the card says what each column holds, a `null: false` one and a nullable one.
        let column = card(&mut harness, &uri, source, "title");
        assert!(column.contains("Story#title -> String\n"), "{column}");
        let nullable = "Story.new.description\n";
        let other = harness.write("app/other.rb", nullable);
        harness.watch(&[&other]);
        let nullable = card(&mut harness, &other, nullable, "description");
        assert!(
            nullable.contains("Story#description -> String?"),
            "{nullable}"
        );
    }

    #[test]
    fn an_array_column_answers_with_an_array_from_either_kind_of_schema() {
        // Both readers at once. Without `array: true`, a Postgres array column would answer its
        // *element* type: a wrong answer, not a missing one, and wrong in a direction that looks
        // right (`story.tags.upcase` would resolve and `story.tags.join` would not). Real
        // `schema.rb` files have plenty of array columns.
        let source = "Story.new.tags.join\n";

        let (mut harness, _schema, uri) = rails_project(source);
        let ruby = card(&mut harness, &uri, source, "join");
        assert!(ruby.contains("Array#join"), "from schema.rb: {ruby}");

        let (mut harness, _dump, uri) = sql_project(source);
        let dumped = card(&mut harness, &uri, source, "join");
        assert!(
            dumped.contains("Array#join"),
            "from structure.sql: {dumped}"
        );
        // And the card on the column itself says there are many, because a hover shows a name, not
        // a return type: the same argument as `null: false`.
        let column = card(&mut harness, &uri, source, "tags");
        assert!(column.contains("Story#tags -> Array"), "{column}");
    }

    #[test]
    fn a_dump_open_in_an_editor_is_read_from_the_buffer() {
        // `with_text` prefers the buffer, so a client whose document selector is wide enough to
        // hand over a `.sql` types its models before it is saved. VS Code's is not (`LANGUAGES` is
        // `ruby` and `erb`), so there the watcher above is the whole story; this is the accessor's
        // other half, and costs nothing to get right.
        let source = "Story.new.title\n";
        let (mut harness, dump, _uri) = sql_project(source);
        assert!(harness.has("Story#title()"));

        harness.open(&dump, STRUCTURE_SQL);
        harness.change(
            &dump,
            "CREATE TABLE public.stories (\n    headline character varying NOT NULL\n);\n",
        );

        assert!(harness.has("Story#headline()"), "the buffer was not read");
        assert!(
            !harness.has("Story#title()"),
            "the column the buffer removed still answers"
        );
    }

    #[test]
    fn a_project_that_excluded_its_db_directory_reads_no_dump() {
        // The feature's one switch, which is the switch everything else has. `index.include` cannot
        // name a `.sql` however spelled, so "not indexed and not read are the same" cannot be the
        // gate here. But `index.exclude` is something the user said, and `Workspace::admits` is
        // that half of `Workspace::indexes`, asked on its own.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("ya-lsp.toml"),
            "[gems]\nenabled = false\n\n[index]\nexclude = [\"db/**/*\"]\n",
        )
        .unwrap();
        let mut harness = Harness::at(dir, PositionEncoding::Utf16);
        harness.write("app/models/story.rb", "class Story\nend\n");
        harness.write("db/structure.sql", STRUCTURE_SQL);
        harness.index();

        assert!(
            !harness.has("Story#title()"),
            "an excluded directory was read anyway"
        );
    }
}
