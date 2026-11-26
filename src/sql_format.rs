//! A conservative SQL formatter for the editor's "Format" (Ctrl+Shift+F).
//!
//! It only moves whitespace and upper-cases reserved words: every other token — identifiers,
//! numbers, string literals, quoted identifiers, comments and `$$` bodies — comes out exactly as
//! it went in, in the same order. Major clauses start a line, the select list and top-level
//! `AND` / `OR` are indented, and statements are separated by a blank line. What sits inside
//! parentheses stays on one line, so a subquery or a function call reads as written.

use crate::sql_highlight::is_keyword;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tok {
    Word(String),
    /// Kept verbatim: strings, quoted identifiers, `$$` bodies, numbers, operators.
    Verbatim(String),
    LineComment(String),
    BlockComment(String),
    Comma,
    Semicolon,
    Open,
    Close,
}

pub(crate) fn lex(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let take_until = |start: usize, end: &dyn Fn(usize) -> Option<usize>| -> (String, usize) {
        let stop = end(start).unwrap_or(chars.len());
        (chars[start..stop].iter().collect(), stop)
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && next == Some('-') {
            let (text, stop) = take_until(i, &|s| (s..chars.len()).find(|&k| chars[k] == '\n'));
            out.push(Tok::LineComment(text.trim_end().to_owned()));
            i = stop;
        } else if c == '/' && next == Some('*') {
            let (text, stop) = take_until(i, &|s| {
                (s + 2..chars.len().saturating_sub(1))
                    .find(|&k| chars[k] == '*' && chars[k + 1] == '/')
                    .map(|k| k + 2)
            });
            out.push(Tok::BlockComment(text));
            i = stop;
        } else if c == '\'' || c == '"' {
            // A doubled quote stays inside the literal.
            let mut k = i + 1;
            while k < chars.len() {
                if chars[k] == c {
                    if chars.get(k + 1) == Some(&c) {
                        k += 2;
                        continue;
                    }
                    break;
                }
                k += 1;
            }
            let stop = (k + 1).min(chars.len());
            out.push(Tok::Verbatim(chars[i..stop].iter().collect()));
            i = stop;
        } else if c == '$' {
            // $tag$ … $tag$ dollar quoting (a function body): untouched.
            let tag_end = (i + 1..chars.len()).find(|&k| chars[k] == '$');
            let tag: Option<String> = tag_end
                .filter(|&e| {
                    chars[i + 1..e]
                        .iter()
                        .all(|ch| ch.is_alphanumeric() || *ch == '_')
                })
                .map(|e| chars[i..=e].iter().collect());
            if let Some(tag) = tag {
                let body_start = i + tag.chars().count();
                let rest: String = chars[body_start..].iter().collect();
                let stop = rest.find(&tag).map_or(chars.len(), |b| {
                    body_start + rest[..b].chars().count() + tag.chars().count()
                });
                out.push(Tok::Verbatim(chars[i..stop].iter().collect()));
                i = stop;
            } else {
                out.push(Tok::Verbatim("$".to_owned()));
                i += 1;
            }
        } else if c.is_alphanumeric() || c == '_' {
            let stop = (i..chars.len())
                .find(|&k| !(chars[k].is_alphanumeric() || chars[k] == '_' || chars[k] == '.'))
                .unwrap_or(chars.len());
            let text: String = chars[i..stop].iter().collect();
            if c.is_ascii_digit() {
                out.push(Tok::Verbatim(text));
            } else {
                out.push(Tok::Word(text));
            }
            i = stop;
        } else {
            out.push(match c {
                ',' => Tok::Comma,
                ';' => Tok::Semicolon,
                '(' => Tok::Open,
                ')' => Tok::Close,
                _ => {
                    // Operators: a run of symbol characters, e.g. `<=`, `::`, `||`.
                    let stop = (i..chars.len())
                        .find(|&k| {
                            let ch = chars[k];
                            ch.is_alphanumeric()
                                || ch.is_whitespace()
                                || "(),;'\"$_".contains(ch)
                                || (ch == '-' && chars.get(k + 1) == Some(&'-'))
                        })
                        .unwrap_or(chars.len())
                        .max(i + 1);
                    let text: String = chars[i..stop].iter().collect();
                    i = stop - 1;
                    Tok::Verbatim(text)
                },
            });
            i += 1;
        }
    }
    out
}

/// Clauses that start a line of their own.
const CLAUSES: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "VALUES",
    "SET",
    "RETURNING",
    "WINDOW",
    "INSERT",
    "UPDATE",
    "DELETE",
    "WITH",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "FULL",
    "CROSS",
    "ON",
    "FETCH",
];

/// Words the highlighter does not list but that read as keywords in an upsert.
const EXTRA_KEYWORDS: &[&str] = &["CONFLICT", "DO", "NOTHING"];

/// Keywords that take a space before a following `(`: `IN (…)`, `VALUES (…)`, `ON CONFLICT (…)`.
const SPACED_BEFORE_PAREN: &[&str] = &[
    "IN",
    "VALUES",
    "AS",
    "ON",
    "USING",
    "EXISTS",
    "ANY",
    "ALL",
    "AND",
    "OR",
    "NOT",
    "WHEN",
    "THEN",
    "ELSE",
    "BY",
    "JOIN",
    "FROM",
    "WHERE",
    "SELECT",
    "INTO",
    "TABLE",
    "WITH",
    "RETURNING",
    "OVER",
    "FILTER",
    "CONFLICT",
    "SET",
    "LATERAL",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "HAVING",
];

/// Format `sql`. Text that does not tokenize cleanly still comes back with every token kept.
#[must_use]
pub fn format(sql: &str) -> String {
    let toks = lex(sql);
    let mut out = String::new();
    let mut depth: i32 = 0;
    // The clause the current line belongs to, at depth 0.
    let mut clause = String::new();
    let mut line_start = true;
    let mut prev: Option<&Tok> = None;
    let newline = |out: &mut String, indent: usize| {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&" ".repeat(indent));
    };
    let mut prev2_upper = String::new();
    for (n, tok) in toks.iter().enumerate() {
        let upper = match tok {
            Tok::Word(w) => w.to_uppercase(),
            _ => String::new(),
        };
        let after = |k: usize| match toks.get(n + k) {
            Some(Tok::Word(w)) => w.to_uppercase(),
            _ => String::new(),
        };
        let before = match prev {
            Some(Tok::Word(w)) => w.to_uppercase(),
            _ => String::new(),
        };
        match tok {
            Tok::Word(w) => {
                // A word called like a function (`count(`, `now(`) is kept as written.
                let called = matches!(toks.get(n + 1), Some(Tok::Open))
                    && !SPACED_BEFORE_PAREN.contains(&upper.as_str());
                let keyword = (is_keyword(w) || EXTRA_KEYWORDS.contains(&upper.as_str()))
                    && !w.contains('.')
                    && !called;
                // A clause word starts a line — but not the second word of a pair already
                // started (`GROUP BY`, `LEFT JOIN`, `DELETE FROM`, `ON CONFLICT`), and not `ON` of
                // a join condition's continuation or a `SET` inside `UPDATE … SET`'s value list.
                let pair_tail = matches!(
                    (before.as_str(), upper.as_str()),
                    (
                        "LEFT" | "RIGHT" | "INNER" | "FULL" | "CROSS" | "OUTER",
                        "JOIN" | "OUTER"
                    ) | ("DELETE", "FROM")
                        | ("INSERT", "INTO")
                        | ("UNION" | "EXCEPT" | "INTERSECT", "ALL")
                ) || (before == "DO" && upper == "UPDATE")
                    || (before == "UPDATE" && upper == "SET" && clause == "DO");
                let starts = depth == 0
                    && keyword
                    && CLAUSES.contains(&upper.as_str())
                    && !pair_tail
                    && !(upper == "FROM" && clause == "DELETE")
                    && !(upper == "SET" && clause == "DO")
                    && !(upper == "UPDATE" && before == "DO")
                    && !(upper == "FULL" && after(1) != "JOIN" && after(1) != "OUTER");
                if starts {
                    newline(&mut out, 0);
                    clause.clone_from(&upper);
                    if upper == "ON" && before == "CONFLICT" {
                        clause = "ON".into();
                    }
                } else if depth == 0
                    && keyword
                    && (upper == "AND" || upper == "OR")
                    && matches!(clause.as_str(), "WHERE" | "HAVING" | "ON")
                {
                    newline(&mut out, 4);
                } else if !line_start
                    && !matches!(prev, Some(Tok::Open))
                    && !matches!(prev, Some(Tok::Verbatim(p)) if p == "::" || p == ".")
                {
                    out.push(' ');
                }
                if upper == "DO" {
                    clause = "DO".into();
                }
                if keyword {
                    out.push_str(&upper);
                } else {
                    out.push_str(w);
                }
                line_start = false;
            },
            Tok::Comma => {
                out.push(',');
                if depth == 0
                    && matches!(
                        clause.as_str(),
                        "SELECT" | "SET" | "GROUP" | "ORDER" | "RETURNING"
                    )
                {
                    newline(&mut out, 4);
                    line_start = true;
                }
            },
            Tok::Semicolon => {
                while out.ends_with(' ') || out.ends_with('\n') {
                    out.pop();
                }
                out.push_str(";\n\n");
                clause.clear();
                depth = 0;
                line_start = true;
            },
            Tok::Open => {
                // `f(` and `t(` hug their paren; keywords and a table after INTO / TABLE do not.
                let hug = matches!(prev, Some(Tok::Word(_)))
                    && !SPACED_BEFORE_PAREN.contains(&before.as_str())
                    && !matches!(prev2_upper.as_str(), "INTO" | "TABLE" | "UPDATE");
                if !line_start && !matches!(prev, Some(Tok::Open)) && !hug {
                    out.push(' ');
                }
                out.push('(');
                depth += 1;
                line_start = false;
            },
            Tok::Close => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push(')');
                depth = (depth - 1).max(0);
            },
            Tok::LineComment(text) => {
                if !line_start {
                    out.push(' ');
                }
                out.push_str(text);
                newline(
                    &mut out,
                    if depth == 0 && clause.is_empty() {
                        0
                    } else {
                        4
                    },
                );
                line_start = true;
            },
            Tok::BlockComment(text) | Tok::Verbatim(text) => {
                let tight = matches!(text.as_str(), "." | "::")
                    || matches!(prev, Some(Tok::Verbatim(p)) if p == "::" || p == ".");
                if !line_start && !matches!(prev, Some(Tok::Open)) && !tight {
                    out.push(' ');
                }
                out.push_str(text);
                line_start = false;
            },
        }
        prev2_upper = before;
        prev = Some(tok);
    }
    let mut result = out.trim_end().to_owned();
    if !result.is_empty() {
        result.push('\n');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokens other than whitespace, keywords compared without case.
    fn shape(sql: &str) -> Vec<Tok> {
        lex(sql)
            .into_iter()
            .map(|t| match t {
                Tok::Word(w)
                    if is_keyword(&w) || EXTRA_KEYWORDS.contains(&w.to_uppercase().as_str()) =>
                {
                    Tok::Word(w.to_uppercase())
                },
                other => other,
            })
            .collect()
    }

    #[test]
    fn clauses_start_lines_and_lists_are_indented() {
        let sql = "select c.id, c.name, count(o.id) as n from customers c left join orders o on o.customer_id = c.id and o.status = 'paid' where c.name like 'A%' or c.id > 10 group by c.id, c.name order by n desc limit 5";
        assert_eq!(
            format(sql),
            "SELECT c.id,\n    c.name,\n    count(o.id) AS n\nFROM customers c\nLEFT JOIN orders o\nON o.customer_id = c.id\n    AND o.status = 'paid'\nWHERE c.name LIKE 'A%'\n    OR c.id > 10\nGROUP BY c.id,\n    c.name\nORDER BY n DESC\nLIMIT 5\n"
        );
    }

    #[test]
    fn strings_identifiers_comments_and_bodies_are_untouched() {
        let sql = "insert into \"Odd Table\" (a) values ('select  from where'); -- keep  me\ncreate function f() returns int language sql as $$ select   1 $$; /* note */ select 1";
        let out = format(sql);
        assert!(out.contains("'select  from where'"));
        assert!(out.contains("\"Odd Table\""));
        assert!(out.contains("-- keep  me"));
        assert!(out.contains("$$ select   1 $$"));
        assert!(out.contains("/* note */"));
        assert_eq!(shape(sql), shape(&out));
    }

    #[test]
    fn statements_are_separated_and_formatting_is_stable() {
        let sql = "update t set a = 1, b = now() where id = 3;delete from t where id=4";
        let once = format(sql);
        assert_eq!(
            once,
            "UPDATE t\nSET a = 1,\n    b = now()\nWHERE id = 3;\n\nDELETE FROM t\nWHERE id = 4\n"
        );
        assert_eq!(format(&once), once);
        assert_eq!(shape(sql), shape(&once));
    }

    #[test]
    fn casts_and_upserts_keep_their_shape() {
        let sql = "insert into t (id, n) values (1, '2'::int) on conflict (id) do update set n = excluded.n";
        let out = format(sql);
        assert!(out.contains("'2'::int"), "{out}");
        assert!(out.contains("DO UPDATE SET n = excluded.n"), "{out}");
        assert_eq!(shape(sql), shape(&out));
    }
}
