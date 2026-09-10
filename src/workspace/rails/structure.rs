//! `db/*structure.sql`, read: the other half of the schema reader, and the only reader here that
//! is not Ruby.
//!
//! An application that sets `config.active_record.schema_format = :sql` has no `db/schema.rb` at
//! all, so [`schema`](super::schema) answers **nothing** about it — a cliff rather than a
//! gradient.
//!
//! # What this is, and what it deliberately is not
//!
//! It is **not** a SQL parser and must not become one. A parser has to understand the whole file,
//! and a real dump holds plpgsql function bodies, triggers, views, enum types and extensions — so
//! a parser that chokes on any one of them loses every table in the file. This is a scanner: it
//! finds `CREATE TABLE` at the top level, reads the body, and **skips everything it does not
//! recognise**, which is the posture every reader in this directory has.
//!
//! Top level is the load-bearing word, and it is the whole of the machinery: [`Scan`] knows the
//! six things that hide text from a scanner — `'…'`, `"…"`, `` `…` ``, `$tag$…$tag$`, `-- …` and
//! `/* … */` — and nothing outside one of those can be mistaken for a table. That is what makes
//! "skips what it does not recognise" true rather than nearly true.
//!
//! # Three dialects, one grammar, and one word that cannot be shared
//!
//! `CREATE TABLE`, a name, parentheses, items separated by commas is the same in all three dumps
//! Rails can produce, and so is the column rule — **a quoted identifier is always a column**,
//! because a dumper quotes exactly what its own dialect reserves. Exactly one token means
//! opposite things:
//!
//! - Postgres does not reserve `key`, so pg_dump writes `key character varying(50) NOT NULL`
//!   bare — a **column**.
//! - MySQL does reserve it, so mysqldump writes a column as `` `key` `` and an index as bare
//!   `KEY name (cols)`.
//!
//! Read it wrong one way and every column named `key` disappears; wrong the other and MySQL's
//! index names are declared as members. So the dialect is detected once per file and decides
//! exactly that one thing — see [`Dialect`].
//!
//! # Where this ends
//!
//! At [`Schema`], which is [`schema`](super::schema)'s. The type table below maps a dump's
//! vocabulary onto **the word the Ruby dumper would have written**, not onto a Ruby class, so
//! `Schema::signatures`, [`rbs_type`](super::schema::rbs_type), the provenance line, the
//! `retyped` withdrawal and every consumer in `analysis::synthesize` are the `.rb` reader's — and
//! the two readers cannot disagree about a database, because only one of them decides what a
//! `string` returns.

use super::schema::{Column, Schema, Table, is_column_name};

/// Which dumper wrote the file, for the one question the answer differs on.
///
/// Detected from the file rather than from its name, because the name is always
/// `structure.sql` whatever produced it. mysqldump's two tells are unmistakable and both appear
/// in every dump it writes: the `/*!…*/` conditional-execution comment, which is MySQL syntax no
/// other database has, and the `ENGINE=` clause after every table.
///
/// It is a substring test rather than a scan, and the **asymmetry is why that is enough**. Two
/// tells that could in principle appear in some other dump's string literal, against one word
/// this decides — so guessing MySQL wrongly drops columns literally named `key`, which fails
/// toward nothing, and guessing standard wrongly declares MySQL's index names as members, which
/// is the bad direction and cannot happen: mysqldump writes both tells unconditionally. Neither
/// string occurs anywhere in discourse's 715 KB, which is the one real dump this crate has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// pg_dump or `sqlite3 .schema`. They differ from each other in vocabulary, which the type
    /// table covers, and in nothing this scanner has to decide.
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

/// Read every table `source` creates. Text in, no graph and no I/O — [`read_schema`]'s contract.
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

/// A body item that is a constraint or an index rather than a column, in any of the dialects.
///
/// Tested against the **unquoted** first word only, which is the rule that makes the list safe
/// to keep short: a dumper quotes a column whose name collides with one of these, so
/// `` `unique` `` and `"check"` are columns and reach none of this. Everything else in a table
/// body is a column — including a generated one, a `PRIMARY KEY` written inline after a column's
/// type, and a type this crate has never seen.
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
/// Only ever consulted for the **first** word, and only for a file a human has edited: every
/// dumper writes a type for every column. `character` is deliberately absent — it is the front
/// of Postgres' own `character varying` — and `with`/`without` are absent for the same reason,
/// since neither can begin a type.
const MODIFIERS: [&str; 7] = [
    "not",
    "null",
    "default",
    "collate",
    "generated",
    "references",
    "as",
];

/// What each dumper's word for a type is in the Ruby dumper's vocabulary.
///
/// The direction is the point. Rails has no table that maps a dump's vocabulary — its
/// `NATIVE_DATABASE_TYPES` is what it *asks* a server for, and pg's read-direction map is keyed
/// on catalog names (`int4`, `float8`, `varchar`) where pg_dump writes the standard spellings —
/// so this table is written here. But it stops at `schema.rb`'s word rather than going on to a
/// Ruby class, which is what makes the two readers unable to disagree: `character varying`
/// becomes `string` and lands wherever `t.string` lands, and if `COLUMN_TYPES` ever grows a row
/// both readers gain it at once.
///
/// A spelling missing from this table is **not** an error and is not dropped. It passes through
/// under its own name, lands on `untyped` exactly as `t.jsonb` does, and is quoted in the
/// provenance line so the card names what the file said. Measured over discourse's 3,355
/// columns: 3,270 map, and six are genuinely unknown — three pgvector `halfvec`, two Postgres
/// enums and an `int4range`.
///
/// One row carries its parentheses and it is not a mistake. MySQL has no boolean: `t.boolean`
/// becomes `tinyint(1)` and `t.integer limit: 1` becomes `tinyint`, so the width is the only
/// thing that tells them apart — which is the same discriminator ActiveRecord's own
/// `emulate_booleans` uses. Lookup therefore tries the spelling with its width first and the
/// spelling without it second.
const TYPES: [(&str, &str); 48] = [
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
    ("timestamp with time zone", "datetime"),
    ("timestamp", "datetime"),
    ("timestamptz", "datetime"),
    ("date", "date"),
    ("time without time zone", "time"),
    ("time with time zone", "time"),
    ("timetz", "time"),
    ("time", "time"),
    // The catalog spellings, for a `structure.sql` a human has edited — which is common in a
    // project that chose this format, and the reason the pass is worth the rows.
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
    // The width of a `t.binary` or a `t.text` picks the type name on MySQL, and all four of
    // each are the same Ruby `String`. Leaving them out is what made `solid_cache`'s
    // `t.binary :value` — a `longblob` — the one column of the nine its three dumps disagreed
    // about, which is what the three-dialect test is for.
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
/// A type is one word unless it is one of these, which is why the list exists rather than a rule
/// about where a type stops: MySQL writes `varchar(255) CHARACTER SET utf8mb4` and Postgres
/// writes `character varying`, so a rule that ended a type at the word `character` would read
/// the first correctly and truncate the second to nothing.
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
    /// The six of them, and every one is a way a scanner reads a table that is not there: a
    /// `CREATE TABLE` inside a plpgsql body, inside a comment mysqldump wrapped a statement in,
    /// or inside a default value's string literal. Unterminated runs to the end of the file,
    /// which is what a truncated dump is and the safe direction — a scanner that recovered would
    /// resume in the middle of a quoted region.
    ///
    /// Bytes rather than `&str` throughout, and not only for speed: this is asked once per byte
    /// of the file by four different scans, and `at` walks a byte at a time — so a UTF-8
    /// continuation byte in a table's comment is an ordinary input here rather than a slice that
    /// panics. Every opener is ASCII, so a position that answers `Some` is always a character
    /// boundary and the callers that go on to read text can do so directly.
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

    /// Where the quoted run opened at `at` ends — one past its closing quote, or the end of the
    /// file if it never closes.
    fn quoted(&self, at: usize, quote: u8) -> usize {
        let mut i = at + 1;
        while i < self.bytes.len() {
            if self.bytes[i] == quote {
                // Doubled is the escape all three dialects use for a quote inside its own
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
            // The first byte before the keyword test, because this runs once per byte of the
            // file and `eq_ignore_ascii_case` on every one of them is most of the scan.
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
            });
        }
        None
    }

    /// The part of a `CREATE …` that says it is a table, and what it is called.
    ///
    /// `None` for a `CREATE` that is not a table's — a sequence, an index, a function — and
    /// `Some(None)` for one that is but whose name or body this scanner will not read, which is
    /// the difference between "keep looking here" and "this was a table and it was skipped".
    #[allow(clippy::option_option)]
    fn table_header(&self, after_create: usize) -> Option<Option<(String, usize)>> {
        let mut at = self.skip(after_create);
        // `GLOBAL`, `LOCAL`, `TEMP`, `TEMPORARY`, `UNLOGGED` — every dumper's modifiers, in any
        // order, because none of them changes what the body means.
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
            // `CREATE TABLE a PARTITION OF b`, or `… (LIKE b)` written without parentheses:
            // there is no body here to read.
            return Some(None);
        }
        // A name with a line break in it would put a second line inside the provenance comment
        // and take the whole generated document down with it — `Synthesized::record` refuses
        // RBS it cannot parse. Nothing narrower is needed, because everything else is inert
        // inside a `#` comment, and nothing narrower is *wanted*: a legacy `structure.sql` is
        // exactly where a table called `OldTable` lives, and `self.table_name =` can claim it.
        if name.contains(['\n', '\r']) {
            return Some(None);
        }
        Some(Some((name, open)))
    }

    /// Whether the word at `at` is `word`, ignoring case, and where it ends.
    ///
    /// `None` at the end of the file as well as on a mismatch, which is the same answer for the
    /// same reason: a dump that stops in the middle of `CREATE` has no table there.
    fn word(&self, at: usize, word: &str) -> Option<usize> {
        let end = at + word.len();
        if !self
            .bytes
            .get(at..end)?
            .eq_ignore_ascii_case(word.as_bytes())
        {
            return None;
        }
        // A keyword only when the next byte cannot continue an identifier, or `CREATE` would
        // match the front of `CREATED_AT`.
        if self.bytes.get(end).is_some_and(|byte| is_name_byte(*byte)) {
            return None;
        }
        Some(end)
    }

    /// Past whitespace and comments — the two things that may sit between any two tokens.
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

    /// The identifier at `at`, unquoted and with any qualification dropped.
    ///
    /// `public.stories` is the table `stories`, which is what Rails calls it; a `"quoted"` or
    /// `` `quoted` `` name is its contents. The last segment is the answer because it is the
    /// name, and the rest of it says where the name lives.
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
                // An unterminated quote at the very end of the file leaves nothing between the
                // two positions, which is the one place these two indices can cross.
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
    /// One walk rather than two, because both questions are the same walk: a type's own
    /// parentheses hold a comma — `numeric(10,2)` — and so does every index and constraint that
    /// lists columns, so depth has to be tracked either way.
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
        // `body` split this item at a comma or a paren it found **outside** every hidden region,
        // so a quoted identifier at the item's start always closes before `to`. The clamp is
        // what makes that an invariant rather than a promise: it costs a comparison, and the
        // alternative is a slice that panics if the split rule ever changes.
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
        let (kind, array) = column_type(&self.source[after..to]);
        Some(Column {
            name,
            kind,
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
    /// Outside, because a default value may hold the words — `DEFAULT 'NOT NULL'` is a string a
    /// column can really have — and a column read as required when it is not is exactly the
    /// wrong direction for a reader whose value is that `null: false` can be believed.
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
/// Postgres' own quoting for a function body, and the reason it exists is the reason this reader
/// has to know it: the text between the tags is arbitrary and can hold anything, `CREATE TABLE`
/// included. A `$` that opens no tag — a `$1` placeholder, a `$` inside an identifier — answers
/// `None` and is an ordinary byte.
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
/// The word `schema.rb` would have used, and whether there are many of them. A spelling this
/// crate does not know passes through under its own name and lands on `untyped` — the same
/// answer, reached the same way, as a `t.jsonb` in a Ruby schema.
fn column_type(rest: &str) -> (String, bool) {
    // A terminator rather than a filter: the first token that cannot be part of a type name ends
    // the type, and if that is the *first* token then the column has none written at all.
    let mut words = rest.split_whitespace();
    let Some(first) = words
        .next()
        .filter(|word| is_type_word(word))
        .map(str::to_ascii_lowercase)
    else {
        return ("untyped".to_owned(), false);
    };
    // A qualified type is an enum's or an extension's — `public.halfvec` — and its schema is
    // noise, exactly as a table's is.
    let mut spelling = first.rsplit('.').next().unwrap_or_default().to_owned();
    if NOT_COLUMNS.contains(&spelling.as_str()) || MODIFIERS.contains(&spelling.as_str()) {
        // A column with no type written at all, which only a hand-edited file has: SQLite
        // takes `"a" NOT NULL`, and reading `not` as the type would put the word in the card.
        return ("untyped".to_owned(), false);
    }
    // One word, unless the words so far are the front of a compound name. The test is made on
    // the bare spelling, because `timestamp(6) without time zone` carries a width in the middle
    // of a name and `character varying[]` carries an array suffix at the end of one.
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
    (mapped.map_or(bare, |(_, ruby)| (*ruby).to_owned()), array)
}

/// Whether `word` can be a type name at all.
///
/// Asked of the **first** token only, and the case it is there for is a real MySQL type rather
/// than a defence: `enum('draft','live')` and `set('a','b')` carry string literals, so they are
/// declined and the column is `untyped` — which is right, because neither is one of
/// `COLUMN_TYPES`' ten and `t.string` is not what ActiveRecord makes of them. Every later token
/// is decided by the compound table instead, which is stricter: no compound type name is made of
/// anything but bare words, so one that is not cannot extend a name either way.
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
    use crate::generated::declaring;
    use std::collections::BTreeMap;

    use super::*;

    /// `solid_cache`'s three-table schema, as **pg_dump** wrote it.
    ///
    /// The gem ships all three of these, which is a controlled experiment nobody had to build:
    /// one migration, three dumpers, and one type name in six spelled the same way by all of
    /// them. They are real files rather than extracts, because half of what this reader has to
    /// do is *skip* — the `SET`s, the `--` comment blocks, the `CREATE SEQUENCE`, the
    /// `ALTER TABLE ... ADD CONSTRAINT`, the `CREATE INDEX` and, in the MySQL one, the `/*!…*/`
    /// wrappers and the `DROP TABLE IF EXISTS`.
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
    /// Every rule that is not shared is visible in this one file: `` `key` `` is a column and
    /// bare `KEY … (…)` two lines below it is an index, `PRIMARY KEY` and `UNIQUE KEY` are body
    /// items that are not columns, `tinyint`/`varbinary`/`longblob` are its own vocabulary, and
    /// `/*!` and `ENGINE=` are what say which dialect this is.
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
    /// The whole file is four lines, every identifier is quoted, and there is no `NOT NULL` on
    /// a column SQLite made the primary key by writing `PRIMARY KEY AUTOINCREMENT` after the
    /// type — which is the shape that makes `says_not_null` a scan of the item rather than a
    /// look at its last two words.
    const SQLITE: &str = r##"CREATE TABLE IF NOT EXISTS "schema_migrations" ("version" varchar NOT NULL PRIMARY KEY);
CREATE TABLE IF NOT EXISTS "ar_internal_metadata" ("key" varchar NOT NULL PRIMARY KEY, "value" varchar, "created_at" datetime(6) NOT NULL, "updated_at" datetime(6) NOT NULL);
CREATE TABLE IF NOT EXISTS "solid_cache_entries" ("id" integer PRIMARY KEY AUTOINCREMENT NOT NULL, "key" blob(1024) NOT NULL, "value" blob(536870912) NOT NULL, "created_at" datetime(6) NOT NULL, "key_hash" integer(8) NOT NULL, "byte_size" integer(4) NOT NULL);
CREATE INDEX "index_solid_cache_entries_on_byte_size" ON "solid_cache_entries" ("byte_size");
CREATE INDEX "index_solid_cache_entries_on_key_hash_and_byte_size" ON "solid_cache_entries" ("key_hash", "byte_size");
CREATE UNIQUE INDEX "index_solid_cache_entries_on_key_hash" ON "solid_cache_entries" ("key_hash");
"##;
    /// Every declaration a dump makes, as `Class#def …` lines, sorted.
    ///
    /// The provenance comment is deliberately dropped: it names the file and the *schema's own
    /// word* for the type, and those are the two things three dumps of one database are allowed
    /// to disagree about. What may not disagree is the member and what it returns.
    fn declarations(source: &str) -> Vec<String> {
        let schema = read_structure(source);
        let classes: BTreeMap<String, Vec<String>> = schema
            .table_names()
            .map(|table| (table.to_owned(), vec![table.to_owned()]))
            .collect();
        let rbs = schema
            .signatures("db/structure.sql", &classes, &BTreeMap::new())
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
            .signatures("db/structure.sql", &classes, &BTreeMap::new())
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

    /// The strongest property this reader has, stated as a test.
    ///
    /// One migration dumped by three databases has to answer with **the same members returning
    /// the same types**, or "one reader, three dialects" is a claim rather than a property. It
    /// holds exactly, including the two things that make it non-trivial: `t.binary` is `bytea`,
    /// `varbinary(1024)`/`longblob` and `blob(1024)`/`blob(536870912)` in the three files, and
    /// `t.datetime` is `timestamp(6) without time zone` in one and `datetime(6)` in the others.
    #[test]
    fn one_schema_dumped_by_three_databases_declares_one_thing() {
        let postgres = declarations(POSTGRES);
        assert_eq!(postgres, declarations(MYSQL));
        assert_eq!(postgres, declarations(SQLITE));
        assert_eq!(
            postgres,
            vec![
                "ar_internal_metadata#created_at: () -> Time",
                "ar_internal_metadata#key: () -> String",
                "ar_internal_metadata#updated_at: () -> Time",
                // The one nullable column in the file, and all three dumps say so the same way.
                "ar_internal_metadata#value: () -> String?",
                "schema_migrations#version: () -> String",
                "solid_cache_entries#byte_size: () -> Integer",
                "solid_cache_entries#created_at: () -> Time",
                "solid_cache_entries#id: () -> Integer",
                // `t.binary`, which is `bytea`, `varbinary(1024)` and `blob(1024)`.
                "solid_cache_entries#key: () -> String",
                "solid_cache_entries#key_hash: () -> Integer",
                // `t.binary` again, and the widths are the whole point: `longblob` and
                // `blob(536870912)` are the same `String` as the 1024-byte one.
                "solid_cache_entries#value: () -> String",
            ]
        );
    }

    /// Where the three dumps *do* differ, said out loud rather than left to be discovered.
    ///
    /// The provenance line quotes the schema's own word, and SQLite has one integer type — so a
    /// `bigint` in the other two dumps is an `integer` there, and no reader can recover the
    /// distinction because the database did not record it. It costs nothing: both are `Integer`
    /// in Ruby, which is what the test above pins. Postgres and MySQL agree on all nine.
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

    /// The one token the dialect decides, in the file that makes both readings visible.
    ///
    /// mysqldump's `solid_cache_entries` has a `` `key` `` column and, five lines below it,
    /// three bare `KEY …` index clauses. Read `KEY` as a column and the table grows three
    /// members named after indexes; read `` `key` `` as a keyword and seven of discourse's
    /// tables lose a real column. Nothing but the dialect flag separates them.
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
        // And the flag really is read from the file rather than assumed: the same body under
        // the other dialect reads the bare `key` as the column Postgres would have meant. It is
        // written lowercase because that is what pg_dump writes — an unquoted identifier is
        // already in canonical form in a dump — and one written `KEY` is declined by the same
        // text rule the Ruby schema applies to a column name, which fails toward nothing.
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
            .signatures("db/structure.sql", &classes, &BTreeMap::new())
            .render(&declaring(&[]))
            .rbs
            .lines()
            .filter_map(|line| line.trim().strip_prefix("def "))
            .filter_map(|line| line.split(':').next())
            .map(str::to_owned)
            .collect()
    }

    /// A quoted identifier is a column whatever it is called, and that rule needs no dialect.
    ///
    /// A dumper quotes exactly what its own dialect reserves, which is why this is the rule
    /// rather than a list of reserved words to keep in step with three databases. Real dumps quote
    /// body identifiers often enough that uppercasing a name before noticing its quotes loses
    /// real columns.
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

    /// Everything a dump holds that is not a table, in one file, around tables that must survive.
    ///
    /// This is the test that stands in for a real DDL parser. A parser has to
    /// understand all of this; a scanner has to *skip* it, and the failure it is skipping is not
    /// hypothetical — a plpgsql body is dollar-quoted text that can hold anything at all,
    /// including the words this scanner looks for.
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
        // A `CREATE TABLE` with no body at all, one whose body never closes, a partition that
        // borrows its columns, and a name with a line break in it — which would put a second
        // line inside the provenance comment and take the whole generated document down.
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
        // A truncated body ends the scan rather than resuming inside it: there is nothing after
        // it in a file that stopped mid-statement, and resuming would read a fragment.
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
        // `id` and `ok`: a name that cannot be written into RBS — the same text rule the Ruby
        // schema applies, and for the same reason — every constraint spelling all three
        // dialects have, an empty item, and one that does not begin with an identifier at all.
        assert_eq!(columns_of(source, "oddities"), vec!["id", "ok"]);
    }

    /// Every spelling in the table, and what each returns. A wrong row is a failing line here.
    ///
    /// The direction is what makes this reviewable: each row maps a *dump's* word onto the word
    /// the Ruby dumper would have written, and `COLUMN_TYPES` — one list, shared with
    /// `db/schema.rb` — decides what that returns. So the two readers cannot disagree about a
    /// database, and a type this crate has never seen passes through under its own name and
    /// lands on `untyped`, which is exactly what `t.jsonb` does.
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
            "boolean",
            "timestamp(6) without time zone",
            "timestamp with time zone",
            "date",
            "time without time zone",
            "integer[]",
            "character varying[]",
            "jsonb",
            "public.some_enum",
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
            let (kind, array) = column_type(declaration);
            (
                declaration,
                super::super::schema::rbs_type(&kind, false, array),
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
            ("boolean", "bool"),
            ("timestamp(6) without time zone", "Time"),
            ("timestamp with time zone", "Time"),
            ("date", "Date"),
            // `t.time` is not one of `COLUMN_TYPES`' ten in a Ruby schema either, and this
            // answers exactly what that answers rather than being cleverer about it.
            ("time without time zone", "untyped"),
            ("integer[]", "Array[Integer]"),
            ("character varying[]", "Array[String]"),
            ("jsonb", "untyped"),
            // A Postgres enum or an extension's type: the schema qualification is noise, the
            // name is quoted in the card, and the answer is the absence of a claim.
            ("public.some_enum", "untyped"),
            ("varchar(255)", "String"),
            ("int", "Integer"),
            // The one row that carries its width, and the reason: MySQL has no boolean.
            ("tinyint(1)", "bool"),
            ("tinyint", "Integer"),
            ("datetime(6)", "Time"),
            ("longblob", "String"),
            ("varbinary(1024)", "String"),
            ("longtext", "String"),
            ("double", "Float"),
            ("decimal(10,2)", "BigDecimal"),
            ("varchar", "String"),
            ("integer(8)", "Integer"),
            ("blob(536870912)", "String"),
            ("float", "Float"),
            ("", "untyped"),
            ("NOT NULL", "untyped"),
            // The shape four of discourse's columns are written in, and the one that made
            // `character varying` look as though it went on forever.
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
            .signatures("db/structure.sql", &classes, &BTreeMap::new())
            .render(&declaring(&[]));
        let optional: Vec<&str> = signatures
            .rbs
            .lines()
            .filter_map(|line| line.trim().strip_prefix("def "))
            .filter(|line| line.ends_with('?'))
            .filter_map(|line| line.split(':').next())
            .collect();
        // `d`'s default is the *string* `NOT NULL`, which a substring search would have read as
        // a constraint — the wrong direction, because a column read as required when it is not
        // is a `String` where the truth is a `String?`.
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
            .signatures("db/structure.sql", &classes, &BTreeMap::new())
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

    /// The dialect is read from the file, and it is the only thing read from the file that is
    /// not a table.
    ///
    /// Both tells, each on its own, and the negative case is the one the substring test rests
    /// on: mysqldump writes both unconditionally, so the reading that would declare an index
    /// name as a member cannot be reached by a real dump, and the reading that costs a column
    /// named `key` fails toward nothing.
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

    /// Everything in a dump that is a byte a scanner has to decide about, in one table.
    ///
    /// Each row is a shape that reached this reader from a real file and would be read wrongly
    /// by one rule fewer: a bare `-` or `/` that opens no comment, a doubled quote, a comment
    /// where whitespace would do, `UNLOGGED`, an identifier that starts with `_` or holds a
    /// `$`, a `NOT` that is not `NOT NULL`, and a column SQLite lets you declare with no type
    /// at all.
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
                .signatures("db/structure.sql", &classes, &BTreeMap::new())
                .render(&declaring(&[]))
                .rbs
                .lines()
                .filter_map(|line| line.trim().strip_prefix("def "))
                .collect::<Vec<_>>(),
            vec![
                "_internal: () -> Integer",
                // `a$b` and `c$d` are **not** here: a `$` is part of an identifier to SQL and
                // opens no dollar-quote — which is the other half of the rule the function
                // bodies rest on — but `def a$b:` is not RBS, so the same text rule the Ruby
                // schema applies declines it. There are two of them because that is what makes
                // the *tag* test run: with one `$` there is no second one to look for, and with
                // two the text between them has to be rejected on its contents.
                "ratio: () -> Integer?",
                // `NOT TRUE` is not `NOT NULL`, and reading it as one would say a nullable
                // column cannot be `nil` — the wrong direction for a nullability claim.
                "flag: () -> bool?",
                "quoted: () -> String",
                // SQLite takes a column with no type written at all; `PRIMARY KEY` is not one.
                "untypedish: () -> untyped",
                // MySQL's inline `enum`, which carries string literals and is declined: it is
                // not one of `COLUMN_TYPES`' ten and `t.string` is not what Rails makes of it.
                "kind: () -> untyped",
            ]
        );
    }

    /// A dollar-quoted body with a name, and a `$` that opens nothing.
    ///
    /// `$$` is what pg_dump writes and what the skipping test uses; `$body$` is what a hand-kept
    /// `structure.sql` writes, and it is the spelling that makes the tag's *contents* matter.
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
        // Unterminated: the body never closes either, so the table is dropped rather than read
        // as a fragment — and there is nothing after it in a file that stopped mid-statement.
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
}
