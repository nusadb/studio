//! Deciding which scripts must be confirmed before they run.
//!
//! Two rules. Statements that destroy data wholesale — `DROP` of a table, database, schema,
//! sequence or role, `TRUNCATE`, `ALTER … DROP COLUMN|CONSTRAINT`, and
//! `DELETE` / `UPDATE` with no `WHERE` — are confirmed everywhere, because one stray Ctrl+Enter
//! is all it takes. On a server tagged production, *every* statement that changes something is
//! confirmed, so the environment tag protects rather than merely decorates.
//!
//! The check reads keywords outside string literals, quoted identifiers and comments, so
//! `SELECT 'drop table'` is not mistaken for a drop.

/// Why a script needs a second look: one line per statement that triggered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Risk {
    pub reasons: Vec<String>,
}

/// The confirmation `statements` need, if any. `production` is the server's environment tag.
#[must_use]
pub fn assess(statements: &[String], production: bool) -> Option<Risk> {
    let mut reasons = Vec::new();
    for sql in statements {
        let words = keywords(sql);
        let first = words.first().map_or("", String::as_str);
        let has = |w: &str| words.iter().any(|x| x == w);
        let reason = match first {
            "DROP" => match words.get(1).map(String::as_str).unwrap_or("") {
                // These hold no rows: dropping one loses a definition that can be recreated
                // (Show DDL has it), so only a production server asks first.
                "INDEX" | "VIEW" | "TRIGGER" | "POLICY" | "FUNCTION" | "PROCEDURE" | "TYPE"
                | "DOMAIN" | "EXTENSION" => {
                    production.then(|| format!("{} on a production server", headline(&words)))
                },
                "SEQUENCE" => Some(format!(
                    "{}: removes it and its current value",
                    headline(&words)
                )),
                "ROLE" | "USER" => Some(format!(
                    "{}: removes the role, its memberships and everything granted to it",
                    headline(&words)
                )),
                _ => Some(format!(
                    "{}: removes it and every row in it",
                    headline(&words)
                )),
            },
            "TRUNCATE" => Some(format!("{}: deletes every row", headline(&words))),
            "DELETE" if !has("WHERE") => Some(format!(
                "{} with no WHERE: deletes every row",
                headline(&words)
            )),
            "UPDATE" if !has("WHERE") => Some(format!(
                "{} with no WHERE: changes every row",
                headline(&words)
            )),
            "ALTER" if has("DROP") => {
                // `DROP DEFAULT` and `DROP NOT NULL` lose nothing; dropping a column or a
                // constraint does.
                let what = words
                    .iter()
                    .zip(words.iter().skip(1))
                    .find(|(a, b)| *a == "DROP" && matches!(b.as_str(), "COLUMN" | "CONSTRAINT"))
                    .map(|(_, b)| b.clone());
                what.map(|what| {
                    format!(
                        "{} … DROP {what}: removes part of the table",
                        headline(&words)
                    )
                })
                .or_else(|| {
                    (production).then(|| format!("{} on a production server", headline(&words)))
                })
            },
            _ if production && writes(first, &words) => {
                Some(format!("{} on a production server", headline(&words)))
            },
            _ => None,
        };
        if let Some(reason) = reason {
            reasons.push(reason);
        }
    }
    (!reasons.is_empty()).then_some(Risk { reasons })
}

/// Whether a statement can change data or schema.
fn writes(first: &str, words: &[String]) -> bool {
    match first {
        "SELECT" | "EXPLAIN" | "SHOW" | "VALUES" | "TABLE" | "BEGIN" | "START" | "COMMIT"
        | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "SET" | "LISTEN" | "UNLISTEN" | "" => {
            false
        },
        // A CTE is read-only unless one of its parts writes.
        "WITH" => words
            .iter()
            .any(|w| matches!(w.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE")),
        _ => true,
    }
}

/// The first few keywords, as a short label: `DELETE FROM`, `DROP TABLE`.
fn headline(words: &[String]) -> String {
    let take = match words.first().map(String::as_str) {
        Some("DELETE" | "DROP" | "TRUNCATE" | "ALTER" | "INSERT" | "CREATE") => 2,
        _ => 1,
    };
    words
        .iter()
        .take(take)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Upper-cased bare words of `sql`, skipping literals, quoted identifiers and comments.
fn keywords(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut words = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' {
            i += 1;
            while i < chars.len() {
                if chars[i] == c {
                    if chars.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            words.push(chars[start..i].iter().collect::<String>().to_uppercase());
        } else {
            i += 1;
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(sql: &str, production: bool) -> Option<Vec<String>> {
        assess(&[sql.to_owned()], production).map(|r| r.reasons)
    }

    #[test]
    fn destructive_statements_are_always_confirmed() {
        assert!(check("DROP TABLE orders", false).is_some());
        assert!(check("DROP INDEX ix_orders_ref", false).is_none());
        assert!(check("DROP INDEX ix_orders_ref", true).is_some());
        assert!(check("DROP ROLE analyst", false).is_some());
        assert!(check("truncate orders", false).is_some());
        assert!(check("DELETE FROM orders", false).is_some());
        assert!(check("update orders set note = null", false).is_some());
        assert!(check("ALTER TABLE orders DROP COLUMN note", false).is_some());
        assert!(check("ALTER TABLE orders DROP CONSTRAINT orders_pkey", false).is_some());
    }

    #[test]
    fn dropping_a_default_or_null_rule_loses_nothing() {
        assert!(check("ALTER TABLE t ALTER COLUMN c DROP DEFAULT", false).is_none());
        assert!(check("ALTER TABLE t ALTER COLUMN c DROP NOT NULL", false).is_none());
        assert!(
            check("ALTER TABLE t ALTER COLUMN c DROP DEFAULT", true).is_some(),
            "but production asks"
        );
    }

    #[test]
    fn scoped_changes_pass_on_a_dev_server() {
        assert!(check("DELETE FROM orders WHERE id = 3", false).is_none());
        assert!(check("UPDATE orders SET note = 'x' WHERE id = 3", false).is_none());
        assert!(check("INSERT INTO orders VALUES (1)", false).is_none());
        assert!(check("ALTER TABLE orders ADD COLUMN x int", false).is_none());
    }

    #[test]
    fn words_inside_literals_and_comments_do_not_count() {
        assert!(check("SELECT 'drop table x'", false).is_none());
        assert!(check("-- DROP TABLE x\nSELECT 1", false).is_none());
        assert!(
            check("DELETE FROM t /* WHERE */", false).is_some(),
            "a commented WHERE is no WHERE"
        );
        assert!(check("UPDATE t SET a = 'where' ", false).is_some());
    }

    #[test]
    fn production_confirms_every_write_but_not_reads() {
        assert!(check("INSERT INTO orders VALUES (1)", true).is_some());
        assert!(check("UPDATE t SET a = 1 WHERE id = 2", true).is_some());
        assert!(check("SELECT * FROM t", true).is_none());
        assert!(check("WITH x AS (SELECT 1) SELECT * FROM x", true).is_none());
        assert!(
            check(
                "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM d",
                true
            )
            .is_some()
        );
        assert!(check("COMMIT", true).is_none());
    }

    #[test]
    fn reasons_name_the_statement() {
        let reasons = check("delete from orders", false).unwrap();
        assert_eq!(
            reasons,
            vec!["DELETE FROM with no WHERE: deletes every row"]
        );
    }
}
