//! Splitting an editor buffer into the statements it holds.
//!
//! The editor runs a whole script, and each statement is sent on its own so that a failure can
//! be pinned to the statement that caused it and the ones after it are not run. The split has
//! to respect everything that can hide a `;` — string literals, quoted identifiers, comments
//! and dollar-quoted bodies — or a semicolon inside `'a;b'` would cut a statement in half.

/// The statements of `script`, trimmed, in order. Empty statements and statements that are
/// nothing but comments are dropped.
#[must_use]
pub fn split_statements(script: &str) -> Vec<String> {
    let mut statements = Vec::new();
    for (start, end) in spans(script) {
        push_statement(&mut statements, &script[start..end]);
    }
    statements
}

/// The statement the caret at byte `offset` belongs to: the one it is inside or right after the
/// `;` of; blank space between statements belongs to the statement below it, and blank space at
/// the end to the last statement. `None` for a script with nothing to run.
#[must_use]
pub fn statement_at(script: &str, offset: usize) -> Option<String> {
    let offset = offset.min(script.len());
    let mut last = None;
    for (start, end) in spans(script) {
        let raw = &script[start..end];
        let has_sql = !is_only_comments(raw.trim());
        // The `;` closing a statement belongs to it; so does the caret right after it.
        let stop = (end + 1).min(script.len());
        if has_sql {
            if offset <= stop && offset >= start {
                return Some(raw.trim().to_owned());
            }
            if start <= offset {
                last = Some(raw.trim().to_owned());
            } else if last.is_none() {
                // Caret before the first statement (on a comment above it).
                return Some(raw.trim().to_owned());
            }
        } else if offset >= start && offset <= stop && last.is_some() {
            return last;
        }
    }
    last
}

/// Byte ranges of the statements of `script`, the separating `;` excluded.
fn spans(script: &str) -> Vec<(usize, usize)> {
    let bytes = script.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => i = skip_quoted(bytes, i),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = bytes[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |n| i + n);
            },
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = script[i + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |n| i + 2 + n + 2);
            },
            b'$' => i = skip_dollar_quoted(script, i),
            b';' => {
                out.push((start, i));
                i += 1;
                start = i;
            },
            _ => i += 1,
        }
    }
    out.push((start, bytes.len()));
    out
}

/// Skip a `'…'` or `"…"` run starting at `open`, honouring doubled quotes as escapes.
fn skip_quoted(bytes: &[u8], open: usize) -> usize {
    let quote = bytes[open];
    let mut i = open + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// Skip a `$tag$ … $tag$` body starting at `open`; a lone `$` (as in `$1`) is just one byte.
fn skip_dollar_quoted(script: &str, open: usize) -> usize {
    let rest = &script[open + 1..];
    let Some(tag_len) = rest.find('$') else {
        return open + 1;
    };
    let tag = &rest[..tag_len];
    if !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || tag.starts_with(|c: char| c.is_ascii_digit())
    {
        return open + 1;
    }
    let delimiter = &script[open..open + tag_len + 2];
    let body = open + delimiter.len();
    script[body..]
        .find(delimiter)
        .map_or(script.len(), |n| body + n + delimiter.len())
}

fn push_statement(statements: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    if !is_only_comments(trimmed) {
        statements.push(trimmed.to_owned());
    }
}

/// `true` when `text` holds no SQL at all once comments and whitespace are removed.
fn is_only_comments(text: &str) -> bool {
    let mut rest = text.trim_start();
    loop {
        if rest.is_empty() {
            return true;
        }
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.find('\n').map_or("", |n| &after[n..]).trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after
                .find("*/")
                .map_or("", |n| &after[n + 2..])
                .trim_start();
        } else {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_caret_picks_its_statement() {
        let script = "SELECT 1;\n-- second\nSELECT 'a;b';\n\nSELECT 3";
        let at = |needle: &str| statement_at(script, script.find(needle).unwrap());
        assert_eq!(at("1;").as_deref(), Some("SELECT 1"));
        assert_eq!(at("'a;b'").as_deref(), Some("-- second\nSELECT 'a;b'"));
        assert_eq!(
            statement_at(script, script.find("'a;b';").unwrap() + 6).as_deref(),
            Some("-- second\nSELECT 'a;b'")
        );
        assert_eq!(
            statement_at(script, script.len()).as_deref(),
            Some("SELECT 3")
        );
        // On a blank line between two statements: the one below, the one being written.
        assert_eq!(
            statement_at(script, script.find("\n\nSELECT 3").unwrap() + 1).as_deref(),
            Some("SELECT 3")
        );
        assert_eq!(statement_at("-- only\n", 3), None);
        assert_eq!(statement_at("  SELECT 9;", 0).as_deref(), Some("SELECT 9"));
    }

    #[test]
    fn statements_are_split_on_semicolons_and_trimmed() {
        assert_eq!(
            split_statements("SELECT 1;\n  SELECT 2 ;\n"),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn a_trailing_statement_without_semicolon_is_kept() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2"),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn semicolons_inside_literals_and_identifiers_do_not_split() {
        assert_eq!(
            split_statements(r#"SELECT 'a;b', "x;y", 'it''s;'; SELECT 3"#),
            vec![r#"SELECT 'a;b', "x;y", 'it''s;'"#, "SELECT 3"]
        );
    }

    #[test]
    fn semicolons_inside_comments_do_not_split() {
        assert_eq!(
            split_statements("SELECT 1 -- not; here\n; /* nor; here */ SELECT 2"),
            vec!["SELECT 1 -- not; here", "/* nor; here */ SELECT 2"]
        );
    }

    #[test]
    fn dollar_quoted_bodies_are_kept_whole_but_parameters_are_not_tags() {
        let script = "CREATE FUNCTION f() RETURNS int AS $body$ SELECT 1; $body$; SELECT $1";
        assert_eq!(
            split_statements(script),
            vec![
                "CREATE FUNCTION f() RETURNS int AS $body$ SELECT 1; $body$",
                "SELECT $1"
            ]
        );
    }

    #[test]
    fn comment_only_chunks_are_dropped() {
        assert_eq!(
            split_statements("-- header\n-- more\n\nSELECT 1;\n-- footer"),
            vec!["-- header\n-- more\n\nSELECT 1"]
        );
        assert!(split_statements("  -- nothing\n /* at all */ ;;").is_empty());
    }
}
