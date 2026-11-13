//! Per-dialect vocabulary for highlighting and autocomplete.
//!
//! Deliberately data, not logic: the shell hands these lists to CodeMirror (or
//! whatever replaces it), so the same words drive syntax highlighting and
//! completion and cannot drift apart.
//!
//! Lists are curated rather than exhaustive. Highlighting a word the server
//! does not know is harmless; *not* highlighting a common keyword looks broken.
//! Everything here is uppercase and sorted, which the tests enforce so callers
//! can binary-search.

use spanners_driver_api::Dialect;

/// Reserved words common to the three supported engines.
const ANSI: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "CHECK",
    "COLUMN",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FETCH",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "REFERENCES",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "SCHEMA",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

const POSTGRES: &[&str] = &[
    "ANALYZE",
    "ARRAY",
    "AUTHORIZATION",
    "BIGSERIAL",
    "BYTEA",
    "CASCADE",
    "CONCURRENTLY",
    "COPY",
    "CUBE",
    "DO",
    "FILTER",
    "GENERATED",
    "GROUPING",
    "ILIKE",
    "JSONB",
    "LATERAL",
    "MATERIALIZED",
    "NULLS",
    "ONLY",
    "OVER",
    "PARTITION",
    "RETURNING",
    "ROLLUP",
    "SERIAL",
    "TABLESAMPLE",
    "TEMP",
    "TEMPORARY",
    "TIMESTAMPTZ",
    "TRUNCATE",
    "UUID",
    "VACUUM",
    "WINDOW",
];

const MYSQL: &[&str] = &[
    "AUTO_INCREMENT",
    "BINARY",
    "CHANGE",
    "CHARSET",
    "COLLATE",
    "DATETIME",
    "DELIMITER",
    "DESCRIBE",
    "DIV",
    "DUPLICATE",
    "ENGINE",
    "EXPLAIN",
    "IGNORE",
    "INT",
    "LONGTEXT",
    "MEDIUMINT",
    "MODIFY",
    "REGEXP",
    "RENAME",
    "REPLACE",
    "RLIKE",
    "SHOW",
    "SIGNED",
    "SMALLINT",
    "TINYINT",
    "UNSIGNED",
    "USE",
    "VARBINARY",
    "VARCHAR",
    "ZEROFILL",
];

const SQLITE: &[&str] = &[
    "ATTACH",
    "AUTOINCREMENT",
    "DETACH",
    "GLOB",
    "INDEXED",
    "INTEGER",
    "ISNULL",
    "NOTNULL",
    "PRAGMA",
    "QUERY",
    "RAISE",
    "REINDEX",
    "ROWID",
    "SAVEPOINT",
    "VACUUM",
    "VIRTUAL",
];

/// Functions worth completing. Not a catalog: the list is what people type and
/// forget the spelling of.
const FUNCTIONS: &[&str] = &[
    "ABS",
    "AVG",
    "COALESCE",
    "CONCAT",
    "COUNT",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DATE",
    "DATE_TRUNC",
    "EXTRACT",
    "GREATEST",
    "LEAST",
    "LENGTH",
    "LOWER",
    "MAX",
    "MIN",
    "NOW",
    "NULLIF",
    "ROUND",
    "STRING_AGG",
    "SUBSTRING",
    "SUM",
    "TRIM",
    "UPPER",
];

/// Types, for `CAST` and column editing.
const TYPES: &[&str] = &[
    "BIGINT",
    "BLOB",
    "BOOLEAN",
    "CHAR",
    "DATE",
    "DECIMAL",
    "DOUBLE",
    "FLOAT",
    "INTEGER",
    "INTERVAL",
    "JSON",
    "NUMERIC",
    "REAL",
    "SMALLINT",
    "TEXT",
    "TIME",
    "TIMESTAMP",
    "VARCHAR",
];

/// Words for syntax highlighting, sorted and deduplicated.
pub fn keywords(dialect: Dialect) -> Vec<&'static str> {
    merged(&[ANSI, dialect_words(dialect)])
}

/// Words for autocomplete: keywords, then built-in functions and types. Order
/// is meaningful for the UI (prefix matches are ranked separately).
pub fn completion_words(dialect: Dialect) -> Vec<&'static str> {
    merged(&[ANSI, dialect_words(dialect), FUNCTIONS, TYPES])
}

/// Case-insensitive keyword check, for the editor's "is this a keyword" query.
pub fn is_keyword(dialect: Dialect, word: &str) -> bool {
    let upper = word.to_ascii_uppercase();
    keywords(dialect).binary_search(&upper.as_str()).is_ok()
}

fn dialect_words(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::Postgres => POSTGRES,
        Dialect::MySql => MYSQL,
        Dialect::Sqlite => SQLITE,
    }
}

fn merged(groups: &[&'static [&'static str]]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = groups.iter().flat_map(|g| g.iter().copied()).collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_DIALECTS: [Dialect; 3] = [Dialect::Postgres, Dialect::MySql, Dialect::Sqlite];

    #[test]
    fn every_list_is_sorted_and_unique_so_binary_search_is_valid() {
        for list in [ANSI, POSTGRES, MYSQL, SQLITE, FUNCTIONS, TYPES] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(list, sorted.as_slice(), "list is not sorted/unique");
        }
    }

    #[test]
    fn every_word_is_uppercase() {
        for list in [ANSI, POSTGRES, MYSQL, SQLITE, FUNCTIONS, TYPES] {
            for word in list {
                assert_eq!(*word, word.to_ascii_uppercase(), "{word} is not uppercase");
            }
        }
    }

    #[test]
    fn common_sql_highlights_in_every_dialect() {
        for d in ALL_DIALECTS {
            for word in ["select", "FROM", "Where", "join", "group"] {
                assert!(is_keyword(d, word), "{word} missing for {d:?}");
            }
        }
    }

    #[test]
    fn dialect_specific_words_stay_in_their_dialect() {
        assert!(is_keyword(Dialect::Postgres, "returning"));
        assert!(!is_keyword(Dialect::MySql, "returning"));
        assert!(is_keyword(Dialect::MySql, "auto_increment"));
        assert!(!is_keyword(Dialect::Postgres, "auto_increment"));
        assert!(is_keyword(Dialect::Sqlite, "pragma"));
        assert!(!is_keyword(Dialect::Postgres, "pragma"));
    }

    #[test]
    fn a_table_name_is_not_a_keyword() {
        assert!(!is_keyword(Dialect::Postgres, "customers"));
        assert!(!is_keyword(Dialect::Sqlite, ""));
    }

    #[test]
    fn completion_lists_include_functions_and_types_but_highlighting_does_not() {
        let completion = completion_words(Dialect::Postgres);
        assert!(completion.contains(&"COALESCE"));
        assert!(completion.contains(&"TIMESTAMP"));

        let keywords = keywords(Dialect::Postgres);
        assert!(!keywords.contains(&"COALESCE"));
        // Types appear in both only when they are also syntax (DATETIME is not).
        assert!(keywords.contains(&"SELECT"));
    }

    #[test]
    fn results_are_deduplicated_across_groups() {
        let words = completion_words(Dialect::MySql);
        let mut unique = words.clone();
        unique.dedup();
        assert_eq!(words, unique, "duplicate completion entries");

        // CURRENT_TIMESTAMP is in both ANSI and FUNCTIONS.
        assert_eq!(
            words.iter().filter(|w| **w == "CURRENT_TIMESTAMP").count(),
            1
        );
    }
}
