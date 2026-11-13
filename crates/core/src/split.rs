//! Splitting a buffer into statements, and finding the one under the cursor.
//!
//! The policy engine can classify a multi-statement buffer, but an editor needs
//! to know *which* statement the user is pointing at: "run the statement under
//! the cursor" is the most-used action in a SQL client, and getting it wrong
//! means running something the user did not mean to run.
//!
//! This is a lexer, not a parser. It only needs to know where a statement ends,
//! so it tracks the constructs that can hide a semicolon: strings, quoted
//! identifiers, comments and Postgres dollar-quoting. Anything it does not
//! understand is treated as ordinary text, which fails toward "one bigger
//! statement" rather than "silently split inside a string".

use spanners_driver_api::Dialect;

/// One statement and where it sits in the original buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// The statement text, trimmed of surrounding whitespace and without the
    /// terminating semicolon.
    pub sql: String,
    /// Byte offset of the first character in the original buffer.
    pub start: usize,
    /// Byte offset one past the last character.
    pub end: usize,
}

impl Statement {
    /// Whether `offset` (a cursor position) falls inside this statement.
    pub fn contains(&self, offset: usize) -> bool {
        offset >= self.start && offset <= self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Normal,
    LineComment,
    BlockComment,
    /// `escape_backslash` is set for MySQL and Postgres `E'...'` strings.
    Single {
        escape_backslash: bool,
    },
    Double,
    Backtick,
    Bracket,
    /// Postgres `$$...$$` / `$tag$...$tag$`; holds the full delimiter.
    Dollar(Vec<u8>),
}

/// Split `sql` into statements.
///
/// Empty statements (stray semicolons, whitespace, comment-only segments) are
/// dropped: the server would ignore them and showing them in a "run all" list
/// would just be noise.
pub fn split(sql: &str, dialect: Dialect) -> Vec<Statement> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut significant = false;
    let mut state = State::Normal;
    let mut i = 0usize;

    while i < bytes.len() {
        let c = bytes[i];

        if !matches!(state, State::Normal) {
            // Inside a construct: look only for its terminator.
            match &state {
                State::LineComment => {
                    if c == b'\n' {
                        state = State::Normal;
                    }
                    i += 1;
                }
                State::BlockComment => {
                    if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        state = State::Normal;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                State::Single { escape_backslash } => {
                    if *escape_backslash && c == b'\\' {
                        i += 2;
                    } else if c == b'\'' {
                        if bytes.get(i + 1) == Some(&b'\'') {
                            i += 2;
                        } else {
                            state = State::Normal;
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                State::Double => {
                    if c == b'"' {
                        if bytes.get(i + 1) == Some(&b'"') {
                            i += 2;
                        } else {
                            state = State::Normal;
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                State::Backtick => {
                    if c == b'`' {
                        if bytes.get(i + 1) == Some(&b'`') {
                            i += 2;
                        } else {
                            state = State::Normal;
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                State::Bracket => {
                    if c == b']' {
                        if bytes.get(i + 1) == Some(&b']') {
                            i += 2;
                        } else {
                            state = State::Normal;
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                State::Dollar(tag) => {
                    if bytes[i..].starts_with(tag) {
                        i += tag.len();
                        state = State::Normal;
                    } else {
                        i += 1;
                    }
                }
                // Excluded by the `!matches!` above.
                State::Normal => unreachable!(),
            }
            continue;
        }

        // Normal state.
        if !c.is_ascii_whitespace() {
            start.get_or_insert(i);
        }

        match c {
            b';' => {
                flush(sql, &mut start, &mut significant, i, &mut out);
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                state = State::LineComment;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                state = State::BlockComment;
                i += 2;
            }
            b'\'' => {
                significant = true;
                state = State::Single {
                    escape_backslash: match dialect {
                        Dialect::MySql => true,
                        // `E'...'` is the Postgres escape-string form; plain
                        // `'...'` does not treat backslash specially.
                        Dialect::Postgres => is_escape_string_prefix(bytes, i),
                        Dialect::Sqlite => false,
                    },
                };
                i += 1;
            }
            b'"' => {
                significant = true;
                state = State::Double;
                i += 1;
            }
            b'`' => {
                significant = true;
                state = State::Backtick;
                i += 1;
            }
            b'[' => {
                significant = true;
                state = State::Bracket;
                i += 1;
            }
            b'$' if dialect == Dialect::Postgres => match dollar_tag(bytes, i) {
                Some(tag) => {
                    significant = true;
                    i += tag.len();
                    state = State::Dollar(tag);
                }
                None => i += 1,
            },
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                significant = true;
                i += 1;
            }
        }
    }

    flush(sql, &mut start, &mut significant, bytes.len(), &mut out);
    out
}

fn flush(
    sql: &str,
    start: &mut Option<usize>,
    significant: &mut bool,
    end: usize,
    out: &mut Vec<Statement>,
) {
    if let Some(s) = start.take() {
        if *significant {
            let raw = &sql[s..end];
            let trimmed = raw.trim_end();
            // `trim_end` only removes whitespace, so the offsets stay valid.
            let end = s + trimmed.len();
            if !trimmed.trim_start().is_empty() {
                out.push(Statement {
                    sql: trimmed.trim_start().to_string(),
                    start: s + (trimmed.len() - trimmed.trim_start().len()),
                    end,
                });
            }
        }
    }
    *significant = false;
}

/// True if the quote at `quote` is preceded by `E`/`e` that is itself not part
/// of an identifier (`table.e'...'` is not an escape string).
fn is_escape_string_prefix(bytes: &[u8], quote: usize) -> bool {
    if quote == 0 {
        return false;
    }
    let prev = bytes[quote - 1];
    if prev != b'E' && prev != b'e' {
        return false;
    }
    match quote.checked_sub(2).map(|i| bytes[i]) {
        Some(c) => !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$'),
        None => true,
    }
}

/// If a dollar-quote delimiter starts at `i`, return it (`$$`, `$tag$`).
///
/// `$1` placeholders are deliberately not matched: a tag cannot start with a
/// digit, so `$1` falls through to ordinary text.
fn dollar_tag(bytes: &[u8], i: usize) -> Option<Vec<u8>> {
    debug_assert_eq!(bytes[i], b'$');
    let mut j = i + 1;
    while j < bytes.len() {
        let c = bytes[j];
        if c == b'$' {
            return Some(bytes[i..=j].to_vec());
        }
        let ok = if j == i + 1 {
            c.is_ascii_alphabetic() || c == b'_'
        } else {
            c.is_ascii_alphanumeric() || c == b'_'
        };
        if !ok {
            return None;
        }
        j += 1;
    }
    None
}

/// The statement containing `offset`.
///
/// A cursor in the whitespace or comments between statements resolves to the
/// next statement, which is what "run the statement I am about to edit" means
/// in practice. A cursor past the last statement resolves to the last one.
pub fn statement_at(sql: &str, offset: usize, dialect: Dialect) -> Option<Statement> {
    let statements = split(sql, dialect);
    if let Some(found) = statements.iter().find(|s| s.contains(offset)) {
        return Some(found.clone());
    }
    if let Some(next) = statements.iter().find(|s| offset < s.start) {
        return Some(next.clone());
    }
    statements.last().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str, dialect: Dialect) -> Vec<String> {
        split(sql, dialect).into_iter().map(|s| s.sql).collect()
    }

    #[test]
    fn splits_on_semicolons() {
        assert_eq!(
            texts("SELECT 1; SELECT 2;SELECT 3", Dialect::Postgres),
            ["SELECT 1", "SELECT 2", "SELECT 3"]
        );
    }

    #[test]
    fn a_single_statement_without_a_semicolon_is_one_statement() {
        assert_eq!(texts("SELECT 1", Dialect::Postgres), ["SELECT 1"]);
    }

    #[test]
    fn ignores_empty_and_comment_only_segments() {
        assert!(texts(";;  ;\n", Dialect::Postgres).is_empty());
        assert!(texts("-- just a note\n", Dialect::Postgres).is_empty());
        assert!(texts("/* nothing here */", Dialect::Postgres).is_empty());
        assert_eq!(
            texts(";; SELECT 1;; -- trailing\n", Dialect::Postgres),
            ["SELECT 1"]
        );
    }

    #[test]
    fn a_semicolon_inside_a_string_does_not_split() {
        assert_eq!(
            texts("SELECT 'a;b'; SELECT 2", Dialect::Postgres),
            ["SELECT 'a;b'", "SELECT 2"]
        );
        // Doubled quotes are an escaped quote, not a terminator.
        assert_eq!(
            texts("SELECT 'it''s; fine'; SELECT 2", Dialect::Postgres),
            ["SELECT 'it''s; fine'", "SELECT 2"]
        );
    }

    #[test]
    fn mysql_backslash_escapes_are_honoured() {
        assert_eq!(
            texts("SELECT 'it\\'s; fine'; SELECT 2", Dialect::MySql),
            ["SELECT 'it\\'s; fine'", "SELECT 2"]
        );
        // Postgres does not treat backslash specially in a plain string, so the
        // quote ends it there and the semicolon is a real separator.
        assert_eq!(
            texts("SELECT 'a\\'; SELECT 2", Dialect::Postgres),
            ["SELECT 'a\\'", "SELECT 2"]
        );
    }

    #[test]
    fn a_postgres_escape_string_does_honour_backslash() {
        assert_eq!(
            texts("SELECT E'it\\'s; fine'; SELECT 2", Dialect::Postgres),
            ["SELECT E'it\\'s; fine'", "SELECT 2"]
        );
    }

    #[test]
    fn a_semicolon_inside_an_identifier_does_not_split() {
        assert_eq!(
            texts("SELECT \"odd;name\" FROM t; SELECT 2", Dialect::Postgres),
            ["SELECT \"odd;name\" FROM t", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT `odd;name` FROM t; SELECT 2", Dialect::MySql),
            ["SELECT `odd;name` FROM t", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT [odd;name] FROM t; SELECT 2", Dialect::Sqlite),
            ["SELECT [odd;name] FROM t", "SELECT 2"]
        );
    }

    #[test]
    fn a_semicolon_inside_a_comment_does_not_split() {
        assert_eq!(
            texts("SELECT 1 -- a; note\n; SELECT 2", Dialect::Postgres),
            ["SELECT 1 -- a; note", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT /* a; note */ 1; SELECT 2", Dialect::Postgres),
            ["SELECT /* a; note */ 1", "SELECT 2"]
        );
    }

    #[test]
    fn dollar_quoted_bodies_do_not_split() {
        assert_eq!(
            texts(
                "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql; SELECT 2",
                Dialect::Postgres
            ),
            [
                "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql",
                "SELECT 2"
            ]
        );
        assert_eq!(
            texts(
                "DO $body$ BEGIN PERFORM 1; END $body$; SELECT 2",
                Dialect::Postgres
            ),
            ["DO $body$ BEGIN PERFORM 1; END $body$", "SELECT 2"]
        );
    }

    #[test]
    fn dollar_placeholders_are_not_dollar_quotes() {
        assert_eq!(
            texts("SELECT $1; SELECT 2", Dialect::Postgres),
            ["SELECT $1", "SELECT 2"]
        );
    }

    #[test]
    fn a_statement_keeps_its_leading_comment() {
        assert_eq!(
            texts("-- explain\nSELECT 1; SELECT 2", Dialect::Postgres),
            ["-- explain\nSELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn offsets_point_at_the_statement_in_the_original_buffer() {
        let sql = "SELECT 1;  SELECT 2";
        let statements = split(sql, Dialect::Postgres);
        assert_eq!(statements[0].start, 0);
        assert_eq!(statements[0].end, 8);
        assert_eq!(&sql[statements[0].start..statements[0].end], "SELECT 1");
        assert_eq!(statements[1].start, 11);
        assert_eq!(&sql[statements[1].start..statements[1].end], "SELECT 2");
    }

    #[test]
    fn statement_at_finds_the_one_under_the_cursor() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3";
        // Inside the first.
        assert_eq!(
            statement_at(sql, 3, Dialect::Postgres).unwrap().sql,
            "SELECT 1"
        );
        // Right after the first semicolon resolves to the next statement.
        assert_eq!(
            statement_at(sql, 9, Dialect::Postgres).unwrap().sql,
            "SELECT 2"
        );
        // Inside the second.
        assert_eq!(
            statement_at(sql, 12, Dialect::Postgres).unwrap().sql,
            "SELECT 2"
        );
        // Past the end resolves to the last statement.
        assert_eq!(
            statement_at(sql, 1_000, Dialect::Postgres).unwrap().sql,
            "SELECT 3"
        );
    }

    #[test]
    fn statement_at_on_an_empty_buffer_is_none() {
        assert!(statement_at("   \n", 0, Dialect::Postgres).is_none());
    }

    #[test]
    fn multibyte_text_keeps_offsets_on_character_boundaries() {
        let sql = "SELECT 'héllo; wörld'; SELECT 2";
        let statements = split(sql, Dialect::Postgres);
        assert_eq!(statements.len(), 2);
        assert_eq!(statements[0].sql, "SELECT 'héllo; wörld'");
        assert_eq!(
            &sql[statements[0].start..statements[0].end],
            "SELECT 'héllo; wörld'"
        );
        assert_eq!(statements[1].sql, "SELECT 2");
    }

    #[test]
    fn an_unterminated_string_does_not_produce_a_bogus_split() {
        // Unterminated quotes are a server error, but the splitter must not
        // invent a second statement out of the remainder.
        assert_eq!(
            texts("SELECT 'oops; SELECT 2", Dialect::Postgres),
            ["SELECT 'oops; SELECT 2"]
        );
    }
}
