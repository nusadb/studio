//! Which table a query result came from, when it came from exactly one: the condition for
//! editing a result in place. Recognised is a plain
//!
//! ```sql
//! SELECT * | col, col, … FROM [schema.]table [[AS] alias] [WHERE …] [ORDER BY …] [LIMIT …] [OFFSET …]
//! ```
//!
//! Anything else (joins, grouping, DISTINCT, set operations, CTEs, expressions or aliases in
//! the select list) gives the reason it cannot be edited instead. Columns are matched to the
//! table by name afterwards, by the caller, together with the primary key.

use crate::sql_format::{Tok, lex};

/// The table a result was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// `None` when the query did not name one.
    pub schema: Option<String>,
    pub table: String,
}

/// The single table `sql` reads, or why its result cannot be edited.
///
/// # Errors
///
/// A short, user-facing reason.
pub fn single_table(sql: &str) -> Result<Source, String> {
    let mut toks: Vec<Tok> = lex(sql)
        .into_iter()
        .filter(|t| !matches!(t, Tok::LineComment(_) | Tok::BlockComment(_)))
        .collect();
    while matches!(toks.last(), Some(Tok::Semicolon)) {
        toks.pop();
    }
    if toks.iter().any(|t| matches!(t, Tok::Semicolon)) {
        return Err("the result comes from a script of several statements".to_owned());
    }
    let mut at = 0;
    if !word_is(toks.first(), "SELECT") {
        return Err("only a SELECT result can be edited".to_owned());
    }
    at += 1;
    if word_is(toks.get(at), "DISTINCT") {
        return Err("DISTINCT rows do not map to table rows".to_owned());
    }
    if word_is(toks.get(at), "ALL") {
        at += 1;
    }

    // Select list, up to FROM: `*`, or bare (optionally qualified) column names.
    let mut items = 0;
    loop {
        match toks.get(at) {
            Some(Tok::Verbatim(v)) if v == "*" => {},
            Some(t) if ident(t).is_some() && !word_is(Some(t), "FROM") => {},
            _ => return Err("the select list has expressions; list plain columns or *".to_owned()),
        }
        // `alias.column` written as a quoted pair: "c"."name".
        at += 1;
        while matches!(toks.get(at), Some(Tok::Verbatim(v)) if v == ".") {
            at += 2;
        }
        items += 1;
        match toks.get(at) {
            Some(Tok::Comma) => at += 1,
            Some(t) if word_is(Some(t), "FROM") => break,
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("AS") => {
                return Err("renamed columns cannot be traced to the table".to_owned());
            },
            Some(Tok::Open) => {
                return Err("the select list has expressions; list plain columns or *".to_owned());
            },
            None => return Err("the query reads no table".to_owned()),
            _ => return Err("the select list has expressions or aliases".to_owned()),
        }
    }
    debug_assert!(items > 0);
    at += 1; // FROM

    // Table name: word (possibly with dots), or quoted parts joined by ".".
    let mut parts = Vec::new();
    match toks.get(at).and_then(ident) {
        Some(first) => parts.extend(first),
        None => return Err("the query does not read a plain table".to_owned()),
    }
    at += 1;
    while matches!(toks.get(at), Some(Tok::Verbatim(v)) if v == ".") {
        match toks.get(at + 1).and_then(ident) {
            Some(more) => parts.extend(more),
            None => return Err("the query does not read a plain table".to_owned()),
        }
        at += 2;
    }
    if matches!(toks.get(at), Some(Tok::Open)) {
        return Err("the query reads a function, not a table".to_owned());
    }
    // Optional alias.
    if word_is(toks.get(at), "AS") {
        at += 1;
    }
    if let Some(t) = toks.get(at)
        && !matches!(t, Tok::Comma)
        && ident(t).is_some()
        && !is_clause(t)
    {
        at += 1;
    }
    // What may follow: WHERE / ORDER BY / LIMIT / OFFSET / FETCH, with anything inside them.
    // Joins, a second table and grouping change what a row is.
    let mut depth = 0usize;
    for t in &toks[at..] {
        match t {
            Tok::Open => depth += 1,
            Tok::Close => depth = depth.saturating_sub(1),
            Tok::Word(w) if depth == 0 => {
                let w = w.to_ascii_uppercase();
                match w.as_str() {
                    "JOIN" | "CROSS" | "NATURAL" | "LATERAL" => {
                        return Err("the result joins several tables".to_owned());
                    },
                    "GROUP" | "HAVING" | "WINDOW" => {
                        return Err("grouped rows do not map to table rows".to_owned());
                    },
                    "UNION" | "INTERSECT" | "EXCEPT" => {
                        return Err("the result combines several queries".to_owned());
                    },
                    _ => {},
                }
            },
            _ => {},
        }
    }
    // A comma straight after the table is an old-style join.
    if matches!(toks.get(at), Some(Tok::Comma)) {
        return Err("the result joins several tables".to_owned());
    }
    if let Some(t) = toks.get(at)
        && !is_clause(t)
    {
        return Err("the query is not a plain read of one table".to_owned());
    }
    let (schema, table) = match parts.len() {
        1 => (None, parts.remove(0)),
        2 => {
            let table = parts.remove(1);
            (Some(parts.remove(0)), table)
        },
        _ => return Err("the query names a table in another database".to_owned()),
    };
    Ok(Source { schema, table })
}

/// A name token as its parts: an unquoted word folds to lower case and may hold dots
/// (`public.orders`), a quoted one keeps its case.
fn ident(t: &Tok) -> Option<Vec<String>> {
    match t {
        Tok::Word(w) => Some(w.split('.').map(str::to_ascii_lowercase).collect()),
        Tok::Verbatim(v) if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') => {
            Some(vec![v[1..v.len() - 1].replace("\"\"", "\"")])
        },
        _ => None,
    }
}

fn word_is(t: Option<&Tok>, word: &str) -> bool {
    matches!(t, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(word))
}

fn is_clause(t: &Tok) -> bool {
    ["WHERE", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR"]
        .iter()
        .any(|c| word_is(Some(t), c))
}

#[cfg(test)]
mod tests {
    use super::{Source, single_table};

    fn ok(sql: &str) -> Source {
        single_table(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    #[test]
    fn plain_reads_of_one_table_are_editable() {
        assert_eq!(
            ok("SELECT * FROM customers ORDER BY id;"),
            Source {
                schema: None,
                table: "customers".into()
            }
        );
        assert_eq!(
            ok("select id, name from Public.Customers where id > 2 limit 10")
                .schema
                .as_deref(),
            Some("public")
        );
        assert_eq!(
            ok("SELECT id, name FROM \"Sales\".\"Orders\" o").table,
            "Orders"
        );
        assert_eq!(
            ok("SELECT c.id, c.name FROM customers AS c WHERE c.id IN (SELECT 1)").table,
            "customers"
        );
        assert_eq!(ok("-- note\nSELECT * FROM t /* x */ ;").table, "t");
    }

    #[test]
    fn anything_that_changes_what_a_row_is_is_read_only() {
        for sql in [
            "SELECT * FROM a JOIN b ON a.id = b.a",
            "SELECT * FROM a, b",
            "SELECT city, count(*) FROM customers GROUP BY city",
            "SELECT DISTINCT city FROM customers",
            "SELECT id AS key FROM customers",
            "SELECT upper(name) FROM customers",
            "SELECT * FROM a UNION SELECT * FROM b",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SELECT 1",
            "SELECT * FROM generate_series(1, 3)",
            "UPDATE t SET a = 1",
            "SELECT * FROM t; SELECT * FROM u",
        ] {
            assert!(single_table(sql).is_err(), "{sql} should be read-only");
        }
    }
}
