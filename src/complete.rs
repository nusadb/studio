//! Completions for the SQL editor.
//!
//! Given the buffer and the caret, this finds the word being typed, works out what can go there
//! — a keyword, a table, or a column (of a named table, or of an alias declared in the
//! statement) — and ranks the candidates. It knows only what the navigator has loaded, so it
//! never suggests a name the database does not have.

/// One suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// What is shown, and inserted.
    pub label: String,
    /// "keyword" | "table" | "view" | "column" — picks the icon.
    pub kind: &'static str,
    /// A short hint beside it: a column's type, a table's schema.
    pub detail: String,
}

/// What the editor knows about the current database.
#[derive(Debug, Clone, Default)]
pub struct Vocabulary {
    /// (schema, name, is_view)
    pub relations: Vec<(String, String, bool)>,
    /// (table, column, type) for tables whose columns have been described.
    pub columns: Vec<(String, String, String)>,
}

/// The word under the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// Byte offset where the replaced text starts.
    pub start: usize,
    /// What has been typed of the name (after any `qualifier.`).
    pub prefix: String,
    /// `orders` in `orders.am|`.
    pub qualifier: Option<String>,
}

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "ILIKE",
    "BETWEEN",
    "ORDER BY",
    "GROUP BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "JOIN",
    "LEFT JOIN",
    "INNER JOIN",
    "ON",
    "AS",
    "DISTINCT",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "RETURNING",
    "CREATE TABLE",
    "ALTER TABLE",
    "DROP TABLE",
    "PRIMARY KEY",
    "REFERENCES",
    "DEFAULT",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "EXPLAIN",
    "WITH",
    "UNION",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "NOW",
    "TRUE",
    "FALSE",
    "ASC",
    "DESC",
    "EXISTS",
    "CAST",
];

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Find the word ending at `caret` (a byte offset into `text`).
#[must_use]
pub fn word_at(text: &str, caret: usize) -> Word {
    let caret = caret.min(text.len());
    let caret = (0..=caret)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    let before = &text[..caret];
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident(*c))
        .last()
        .map_or(caret, |(i, _)| i);
    let prefix = before[start..].to_owned();
    let qualifier = before[..start].strip_suffix('.').and_then(|head| {
        let q_start = head
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_ident(*c) || *c == '"')
            .last()
            .map(|(i, _)| i)?;
        let q = head[q_start..].trim_matches('"');
        (!q.is_empty()).then(|| q.to_owned())
    });
    Word {
        start,
        prefix,
        qualifier,
    }
}

/// Map aliases declared with `FROM t a`, `JOIN t AS a` in `text` to their tables.
fn aliases(text: &str) -> Vec<(String, String)> {
    let words: Vec<String> = text
        .split(|c: char| !(is_ident(c) || c == '.' || c == '"'))
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_matches('"').to_owned())
        .collect();
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if matches!(
            w.to_uppercase().as_str(),
            "FROM" | "JOIN" | "UPDATE" | "INTO"
        ) && let Some(table) = words.get(i + 1)
        {
            let table = table.rsplit('.').next().unwrap_or(table).to_owned();
            let mut next = i + 2;
            if words
                .get(next)
                .is_some_and(|w| w.eq_ignore_ascii_case("AS"))
            {
                next += 1;
            }
            if let Some(alias) = words.get(next)
                && !KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(alias))
                && !matches!(
                    alias.to_uppercase().as_str(),
                    "WHERE"
                        | "LEFT"
                        | "RIGHT"
                        | "INNER"
                        | "ON"
                        | "SET"
                        | "ORDER"
                        | "GROUP"
                        | "LIMIT"
                )
            {
                out.push((alias.clone(), table.clone()));
            }
            out.push((table.clone(), table));
        }
    }
    out
}

/// Tables named after FROM / JOIN / UPDATE / INTO in `text` — whose columns are worth loading.
#[must_use]
pub fn referenced_tables(text: &str) -> Vec<String> {
    let mut tables: Vec<String> = aliases(text).into_iter().map(|(_, t)| t).collect();
    tables.sort();
    tables.dedup();
    tables
}

/// The keyword just before the word, which says what kind of name belongs there.
fn context(text: &str, start: usize) -> String {
    text[..start]
        .split(|c: char| !is_ident(c))
        .rfind(|w| !w.is_empty())
        .unwrap_or("")
        .to_uppercase()
}

/// Ranked suggestions for the word at `caret`, at most `limit` of them.
#[must_use]
pub fn suggest(
    text: &str,
    caret: usize,
    vocab: &Vocabulary,
    limit: usize,
) -> (Word, Vec<Suggestion>) {
    let word = word_at(text, caret);
    let needle = word.prefix.to_lowercase();
    let matches = |name: &str| name.to_lowercase().starts_with(&needle);
    let mut out: Vec<(u8, Suggestion)> = Vec::new();

    if let Some(qualifier) = &word.qualifier {
        let table = aliases(text)
            .into_iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(qualifier))
            .map_or_else(|| qualifier.clone(), |(_, t)| t);
        for (t, column, kind) in &vocab.columns {
            if t.eq_ignore_ascii_case(&table) && matches(column) {
                out.push((
                    0,
                    Suggestion {
                        label: column.clone(),
                        kind: "column",
                        detail: kind.clone(),
                    },
                ));
            }
        }
    } else {
        let after = context(text, word.start);
        let wants_table = matches!(
            after.as_str(),
            "FROM" | "JOIN" | "UPDATE" | "INTO" | "TABLE"
        );
        for (schema, name, is_view) in &vocab.relations {
            if matches(name) {
                let rank = if wants_table { 0 } else { 2 };
                out.push((
                    rank,
                    Suggestion {
                        label: name.clone(),
                        kind: if *is_view { "view" } else { "table" },
                        detail: schema.clone(),
                    },
                ));
            }
        }
        if !wants_table {
            let in_play = referenced_tables(text);
            for (table, column, kind) in &vocab.columns {
                if in_play.iter().any(|t| t.eq_ignore_ascii_case(table)) && matches(column) {
                    out.push((
                        1,
                        Suggestion {
                            label: column.clone(),
                            kind: "column",
                            detail: format!("{table} · {kind}"),
                        },
                    ));
                }
            }
        }
        // Keywords complete only once something is typed, so they never bury names.
        if !needle.is_empty() {
            for k in KEYWORDS {
                if k.to_lowercase().starts_with(&needle) && !k.eq_ignore_ascii_case(&word.prefix) {
                    out.push((
                        if wants_table { 4 } else { 3 },
                        Suggestion {
                            label: (*k).to_owned(),
                            kind: "keyword",
                            detail: String::new(),
                        },
                    ));
                }
            }
        }
    }

    out.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.label.len().cmp(&b.1.label.len()))
            .then(a.1.label.cmp(&b.1.label))
    });
    out.dedup_by(|a, b| a.1.label == b.1.label && a.1.kind == b.1.kind);
    // A single suggestion that is exactly what was typed offers nothing.
    let list: Vec<Suggestion> = out.into_iter().map(|(_, s)| s).take(limit).collect();
    if list.len() == 1 && list[0].label == word.prefix {
        return (word, Vec::new());
    }
    (word, list)
}

/// The buffer with the word replaced by `label`, and the caret after it.
#[must_use]
pub fn apply(text: &str, word: &Word, caret: usize, label: &str) -> (String, usize) {
    let caret = caret.min(text.len()).max(word.start);
    let mut out = String::with_capacity(text.len() + label.len());
    out.push_str(&text[..word.start]);
    out.push_str(label);
    out.push_str(&text[caret..]);
    (out, word.start + label.len())
}

/// Line and column (in characters) of a byte offset — where the popup goes.
#[must_use]
pub fn line_col(text: &str, caret: usize) -> (usize, usize) {
    let caret = caret.min(text.len());
    let before = &text[..(0..=caret)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0)];
    let line = before.matches('\n').count();
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count());
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> Vocabulary {
        Vocabulary {
            relations: vec![
                ("public".into(), "orders".into(), false),
                ("public".into(), "order_lines".into(), false),
                ("public".into(), "customers".into(), false),
                ("public".into(), "v_top".into(), true),
            ],
            columns: vec![
                ("orders".into(), "id".into(), "int".into()),
                ("orders".into(), "amount".into(), "numeric".into()),
                ("customers".into(), "name".into(), "text".into()),
            ],
        }
    }

    fn labels(text: &str) -> Vec<String> {
        suggest(text, text.len(), &vocab(), 10)
            .1
            .into_iter()
            .map(|s| s.label)
            .collect()
    }

    #[test]
    fn the_word_under_the_caret_is_found_with_its_qualifier() {
        assert_eq!(
            word_at("SELECT o.am", 11),
            Word {
                start: 9,
                prefix: "am".into(),
                qualifier: Some("o".into())
            }
        );
        assert_eq!(
            word_at("SELECT ", 7),
            Word {
                start: 7,
                prefix: String::new(),
                qualifier: None
            }
        );
        assert_eq!(
            word_at("SELECT \"orders\".", 16).qualifier.as_deref(),
            Some("orders")
        );
    }

    #[test]
    fn after_from_tables_come_first() {
        assert_eq!(
            labels("SELECT * FROM ord"),
            vec!["orders", "order_lines", "ORDER BY"]
        );
    }

    #[test]
    fn a_qualifier_lists_that_tables_columns_even_through_an_alias() {
        assert_eq!(labels("SELECT orders."), vec!["id", "amount"]);
        let text = "SELECT o.a FROM orders o";
        let (_, list) = suggest(text, 10, &vocab(), 10);
        assert_eq!(
            list.into_iter().map(|s| s.label).collect::<Vec<_>>(),
            vec!["amount"]
        );
    }

    #[test]
    fn columns_of_tables_in_play_are_offered_unqualified() {
        let text = "SELECT am FROM orders";
        let (_, list) = suggest(text, 9, &vocab(), 10);
        assert_eq!(list[0].label, "amount");
        assert_eq!(list[0].kind, "column");
    }

    #[test]
    fn keywords_need_a_prefix_and_an_exact_match_offers_nothing() {
        assert!(
            labels("SELECT * FROM orders WHERE ")
                .iter()
                .all(|l| l != "AND")
        );
        assert!(labels("SELECT * FROM orders wh").contains(&"WHERE".to_owned()));
        assert!(labels("SELECT * FROM orders").is_empty());
    }

    #[test]
    fn applying_replaces_the_typed_prefix_and_moves_the_caret() {
        let text = "SELECT * FROM ord WHERE";
        let word = word_at(text, 17);
        assert_eq!(
            apply(text, &word, 17, "orders"),
            ("SELECT * FROM orders WHERE".to_owned(), 20)
        );
    }

    #[test]
    fn line_and_column_count_characters() {
        assert_eq!(line_col("ab\nçd", 5), (1, 1), "ç is two bytes");
        assert_eq!(line_col("ab\nçd", 6), (1, 2));
        assert_eq!(line_col("", 0), (0, 0));
    }
}
