//! Autocomplete, from the live schema plus the words the dialect knows.
//!
//! Completion is a ranking problem, not a parsing one. The editor asks "what
//! could go here?" and the useful answers are, in order: the columns of the
//! tables this statement already mentions, then tables, then functions, then
//! keywords. Anything cleverer than that starts guessing, and a SQL client
//! that offers the wrong column is worse than one that offers none.
//!
//! Everything here is a heuristic over text. Nothing in this module decides
//! whether SQL is safe to run; that is [`crate::policy`]'s job.

use spanners_driver_api::{Dialect, SchemaSnapshot, TableKind};

use crate::dialect;
use crate::split;

/// What a suggestion is, so the UI can pick an icon and a sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Column,
    Table,
    View,
    Function,
    Type,
    Keyword,
}

/// One suggestion. `text` is what gets inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub kind: Kind,
    /// Extra context for the popup, e.g. the column type or the table's schema.
    pub detail: Option<String>,
}

/// Suggestions for the cursor at `offset` in `sql`.
///
/// `schema` may be empty (not connected yet): keywords and functions still
/// work, which keeps the editor useful while a connection is opening.
pub fn complete(
    schema: &SchemaSnapshot,
    dialect: Dialect,
    sql: &str,
    offset: usize,
) -> Vec<Completion> {
    let offset = offset.min(sql.len());
    let start = word_start(sql, offset);
    let typed = &sql[start..offset];
    let statement = split::statement_at(sql, offset, dialect);
    let scope = statement
        .as_ref()
        .map(|s| referenced_tables(&s.sql))
        .unwrap_or_default();

    let mut out: Vec<Completion> = Vec::new();

    if let Some(qualifier) = qualifier_before(sql, start) {
        // `alias.` / `table.` / `schema.`: only that object's columns, or that
        // schema's tables. Offering the whole dictionary here would bury the
        // one thing the user is looking for.
        let mut matched = false;
        if let Some(table) = resolve_qualifier(schema, &scope, &qualifier) {
            matched = true;
            for column in &table.columns {
                out.push(Completion {
                    text: column.name.clone(),
                    kind: Kind::Column,
                    detail: Some(column.type_name.clone()),
                });
            }
        }
        if !matched {
            if let Some(s) = schema.schemas.iter().find(|s| s.name == qualifier) {
                matched = true;
                for table in &s.tables {
                    out.push(Completion {
                        text: table.name.clone(),
                        kind: kind_of(table.kind),
                        detail: Some(s.name.clone()),
                    });
                }
            }
        }
        if matched {
            return rank(out, typed);
        }
        // An unknown qualifier falls through to the general list: the user may
        // still be typing a name that does not exist yet (a CTE, say).
    }

    // Columns of the tables this statement names come first, because that is
    // almost always what is being typed.
    let mut seen = std::collections::BTreeSet::new();
    for name in &scope {
        if let Some(table) = find_table(schema, name) {
            for column in &table.columns {
                if seen.insert(column.name.to_ascii_lowercase()) {
                    out.push(Completion {
                        text: column.name.clone(),
                        kind: Kind::Column,
                        detail: Some(column.type_name.clone()),
                    });
                }
            }
        }
    }

    for s in &schema.schemas {
        for table in &s.tables {
            out.push(Completion {
                text: table.name.clone(),
                kind: kind_of(table.kind),
                detail: Some(s.name.clone()),
            });
        }
    }

    for word in dialect::completion_words(dialect) {
        let kind = if dialect::is_function(dialect, word) {
            Kind::Function
        } else if dialect::is_type(dialect, word) {
            Kind::Type
        } else {
            Kind::Keyword
        };
        out.push(Completion {
            text: word.to_string(),
            kind,
            detail: None,
        });
    }

    rank(out, typed)
}

fn kind_of(kind: TableKind) -> Kind {
    match kind {
        TableKind::Table => Kind::Table,
        TableKind::View => Kind::View,
        TableKind::Other => Kind::Table,
    }
}

/// Prefix matches first, then substring matches; within a group, columns
/// before tables before functions before keywords, then shortest, then
/// alphabetical. Deduplicated case-insensitively so `id` does not appear twice.
fn rank(candidates: Vec<Completion>, typed: &str) -> Vec<Completion> {
    let needle = typed.to_ascii_lowercase();
    let mut scored: Vec<(u8, Kind, usize, String, Completion)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for c in candidates {
        let haystack = c.text.to_ascii_lowercase();
        let score = if needle.is_empty() || haystack.starts_with(&needle) {
            0
        } else if haystack.contains(&needle) {
            1
        } else {
            continue;
        };
        if !seen.insert((haystack.clone(), c.kind)) {
            continue;
        }
        scored.push((score, c.kind, c.text.len(), haystack, c));
    }
    scored.sort_by(|a, b| (a.0, a.1, a.2, &a.3).cmp(&(b.0, b.1, b.2, &b.3)));
    scored.into_iter().map(|(.., c)| c).collect()
}

/// Start of the identifier being typed at `offset`.
fn word_start(sql: &str, offset: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut i = offset;
    while i > 0 {
        let c = bytes[i - 1];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

/// The word before a `.` immediately preceding `start`, if any.
fn qualifier_before(sql: &str, start: usize) -> Option<String> {
    let bytes = sql.as_bytes();
    if start == 0 || bytes[start - 1] != b'.' {
        return None;
    }
    let mut i = start - 1;
    while i > 0 {
        let c = bytes[i - 1];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
            i -= 1;
        } else {
            break;
        }
    }
    if i == start - 1 {
        None
    } else {
        Some(sql[i..start - 1].to_string())
    }
}

/// A table named in the statement, by name or alias.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reference {
    /// The name as written, which may be schema-qualified.
    name: String,
    alias: Option<String>,
}

/// Tables and aliases mentioned by `FROM`, `JOIN`, `UPDATE` and `INSERT INTO`.
///
/// Deliberately textual: this only affects which columns are offered, so a
/// miss here costs a suggestion, never a wrong statement.
fn referenced_tables(sql: &str) -> Vec<String> {
    let words = words(sql);
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let upper = words[i].to_ascii_uppercase();
        let is_source = matches!(
            upper.as_str(),
            "FROM" | "JOIN" | "UPDATE" | "INTO" | "TABLE"
        );
        if is_source {
            if let Some((reference, next)) = read_reference(&words, i + 1) {
                out.push(reference.name.clone());
                // Record the alias too, so `FROM users u ... u.` works.
                if let Some(alias) = reference.alias {
                    out.push(alias);
                }
                i = next;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Read `name` or `schema.name`, plus an optional alias.
fn read_reference(words: &[String], mut i: usize) -> Option<(Reference, usize)> {
    // Skip `ONLY`, `LATERAL` and similar noise words.
    while i < words.len() && matches!(words[i].to_ascii_uppercase().as_str(), "ONLY" | "LATERAL") {
        i += 1;
    }
    if i >= words.len() || !is_ident(&words[i]) {
        return None;
    }
    let mut name = words[i].clone();
    i += 1;
    if i + 1 < words.len() && words[i] == "." && is_ident(&words[i + 1]) {
        name = format!("{name}.{}", words[i + 1]);
        i += 2;
    }
    let alias = if i < words.len() && words[i].eq_ignore_ascii_case("AS") {
        if i + 1 < words.len() && is_ident(&words[i + 1]) {
            let a = words[i + 1].clone();
            i += 2;
            Some(a)
        } else {
            None
        }
    } else if i < words.len() && is_ident(&words[i]) && !is_clause_word(&words[i]) {
        let a = words[i].clone();
        i += 1;
        Some(a)
    } else {
        None
    };
    Some((Reference { name, alias }, i))
}

fn is_clause_word(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "WHERE"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "CROSS"
            | "ON"
            | "USING"
            | "GROUP"
            | "ORDER"
            | "LIMIT"
            | "OFFSET"
            | "HAVING"
            | "UNION"
            | "RETURNING"
            | "SET"
            | "VALUES"
            | "SELECT"
            | "WINDOW"
            | "FOR"
    )
}

fn is_ident(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Words of a statement, with quoted spans skipped.
///
/// Only used for alias detection, so it does not try to be a lexer: a comment
/// may contribute words, which at worst offers a column that was already
/// visible in the comment.
fn words(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' | b'"' | b'`' => {
                let quote = c;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        if bytes.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
                {
                    i += 1;
                }
                out.push(sql[start..i].to_string());
            }
            _ => i += 1,
        }
    }
    out
}

fn resolve_qualifier<'a>(
    schema: &'a SchemaSnapshot,
    scope: &[String],
    qualifier: &str,
) -> Option<&'a spanners_driver_api::Table> {
    // `FROM users u ... u.` — find which table `u` stands for.
    let words = scope;
    if let Some(name) = words
        .windows(2)
        .find(|w| w[1].eq_ignore_ascii_case(qualifier))
        .map(|w| w[0].clone())
    {
        if let Some(table) = find_table(schema, &name) {
            return Some(table);
        }
    }
    if let Some(table) = find_table(schema, qualifier) {
        return Some(table);
    }
    // Last resort: a unique table name, so `users.` works without a FROM.
    let mut found = None;
    for s in &schema.schemas {
        for table in &s.tables {
            if table.name.eq_ignore_ascii_case(qualifier) {
                if found.is_some() {
                    return None;
                }
                found = Some(table);
            }
        }
    }
    found
}

/// Find a table by `name` or `schema.name`, preferring the default schema.
fn find_table<'a>(
    schema: &'a SchemaSnapshot,
    name: &str,
) -> Option<&'a spanners_driver_api::Table> {
    let (schema_name, table_name) = match name.split_once('.') {
        Some((s, t)) => (Some(s), t),
        None => (None, name),
    };
    let mut fallback = None;
    for s in &schema.schemas {
        if let Some(want) = schema_name {
            if !s.name.eq_ignore_ascii_case(want) {
                continue;
            }
        }
        for table in &s.tables {
            if table.name.eq_ignore_ascii_case(table_name) {
                if schema_name.is_some() {
                    return Some(table);
                }
                // Prefer `public`/`main`/the database's own schema over the
                // others when the name is ambiguous.
                if is_default_schema(&s.name) {
                    return Some(table);
                }
                fallback.get_or_insert(table);
            }
        }
    }
    fallback
}

fn is_default_schema(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "public" | "main" | "dbo"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use spanners_driver_api::{Schema, Table, TableColumn};

    fn column(name: &str, type_name: &str) -> TableColumn {
        TableColumn {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
        }
    }

    fn schema() -> SchemaSnapshot {
        SchemaSnapshot {
            schemas: vec![Schema {
                name: "public".into(),
                tables: vec![
                    Table {
                        schema: "public".into(),
                        name: "users".into(),
                        kind: TableKind::Table,
                        columns: vec![column("id", "int4"), column("email", "text")],
                    },
                    Table {
                        schema: "public".into(),
                        name: "orders".into(),
                        kind: TableKind::Table,
                        columns: vec![column("id", "int4"), column("user_id", "int4")],
                    },
                    Table {
                        schema: "public".into(),
                        name: "user_totals".into(),
                        kind: TableKind::View,
                        columns: vec![column("total", "numeric")],
                    },
                ],
            }],
        }
    }

    fn texts(list: &[Completion]) -> Vec<String> {
        list.iter().map(|c| c.text.clone()).collect()
    }

    fn at(sql: &str, marker: &str) -> usize {
        sql.find(marker).unwrap() + marker.len()
    }

    #[test]
    fn columns_of_the_tables_in_the_statement_come_first() {
        let sql = "SELECT  FROM users";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "SELECT "));
        assert_eq!(out[0].text, "id");
        assert_eq!(out[0].kind, Kind::Column);
        assert!(out.iter().any(|c| c.text == "email"));
        // Columns of a table that is not named here still appear, but later.
        let id_pos = out.iter().position(|c| c.text == "id").unwrap();
        let kw_pos = out.iter().position(|c| c.text == "SELECT").unwrap();
        assert!(id_pos < kw_pos, "columns should outrank keywords");
    }

    #[test]
    fn a_table_alias_scopes_the_columns() {
        let sql = "SELECT u. FROM users u";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "u."));
        // Same kind, so the shorter name sorts first.
        assert_eq!(texts(&out), ["id", "email"]);
        assert!(out.iter().all(|c| c.kind == Kind::Column));
    }

    #[test]
    fn a_table_name_scopes_the_columns() {
        let sql = "SELECT orders. FROM orders";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "orders."));
        assert_eq!(texts(&out), ["id", "user_id"]);
    }

    #[test]
    fn a_schema_name_offers_its_tables() {
        let sql = "SELECT * FROM public.";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "public."));
        // Tables before views; within a kind, shortest first.
        assert_eq!(texts(&out), ["users", "orders", "user_totals"]);
    }

    #[test]
    fn typing_a_prefix_filters_and_puts_prefix_matches_first() {
        // `orders` is in scope, so its `user_id` column outranks the tables
        // that merely share the prefix.
        let sql = "SELECT use FROM orders";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "SELECT use"));
        assert_eq!(out[0].text, "user_id");
        assert!(texts(&out).contains(&"users".to_string()));
        assert!(texts(&out).contains(&"user_totals".to_string()));
        // Columns the prefix does not match are filtered out entirely.
        assert!(!texts(&out).contains(&"id".to_string()));
    }

    #[test]
    fn a_prefix_match_outranks_a_substring_match() {
        // `MAX` starts with "ma"; `email` only contains it.
        let sql = "SELECT ma FROM users";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "SELECT ma"));
        let max = out.iter().position(|c| c.text == "MAX").unwrap();
        let email = out.iter().position(|c| c.text == "email").unwrap();
        assert!(max < email, "{:?}", texts(&out));
    }

    #[test]
    fn an_empty_schema_still_completes_keywords() {
        let sql = "SEL";
        let out = complete(
            &SchemaSnapshot::default(),
            Dialect::Sqlite,
            sql,
            at(sql, "SEL"),
        );
        assert_eq!(out[0].text, "SELECT");
        assert_eq!(out[0].kind, Kind::Keyword);
    }

    #[test]
    fn views_are_labelled_as_views() {
        let sql = "SELECT * FROM user_totals";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "SELECT *"));
        let view = out.iter().find(|c| c.text == "user_totals").unwrap();
        assert_eq!(view.kind, Kind::View);
    }

    #[test]
    fn keywords_and_functions_are_told_apart() {
        let sql = "SE";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "SE"));
        assert_eq!(
            out.iter().find(|c| c.text == "SELECT").unwrap().kind,
            Kind::Keyword
        );
        let sql = "COAL";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "COAL"));
        assert_eq!(
            out.iter().find(|c| c.text == "COALESCE").unwrap().kind,
            Kind::Function
        );
    }

    #[test]
    fn an_unknown_qualifier_falls_back_to_the_general_list() {
        // A CTE that the schema does not know about yet.
        let sql = "WITH recent AS (SELECT 1) SELECT recent.";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "recent."));
        assert!(!out.is_empty());
        assert!(out.iter().any(|c| c.text == "SELECT"));
    }

    #[test]
    fn a_cursor_in_whitespace_uses_the_neighbouring_statement() {
        let sql = "SELECT 1;\n\nSELECT  FROM users";
        let out = complete(&schema(), Dialect::Postgres, sql, sql.find("FROM").unwrap());
        assert_eq!(out[0].text, "id");
    }

    #[test]
    fn aliases_are_not_read_from_strings_or_comments() {
        let sql = "SELECT 'FROM secret s' FROM users -- FROM nope n\n";
        let out = complete(&schema(), Dialect::Postgres, sql, sql.len());
        assert!(out.iter().any(|c| c.text == "email"));
        // `nope` is not a table, so nothing breaks; the point is that the
        // parser did not invent a table called `secret`.
        assert!(!texts(&out).contains(&"secret".to_string()));
    }

    #[test]
    fn a_qualified_table_name_in_from_resolves() {
        let sql = "SELECT public.users. FROM public.users";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "users."));
        assert_eq!(texts(&out), ["id", "email"]);
    }

    #[test]
    fn the_same_column_from_two_tables_appears_once() {
        let sql = "SELECT  FROM users, orders";
        let out = complete(&schema(), Dialect::Postgres, sql, at(sql, "SELECT "));
        assert_eq!(out.iter().filter(|c| c.text == "id").count(), 1);
    }

    #[test]
    fn a_multibyte_cursor_does_not_panic() {
        let sql = "SELECT 'héllo' FROM users WHERE ";
        let out = complete(&schema(), Dialect::Postgres, sql, sql.len());
        assert!(!out.is_empty());
    }
}
