//! Token classification for the SQL editor.
//!
//! The editor colours code the way the visual reference does — per token, not per line — so
//! this module splits a line into runs and labels each one. It is a lexer for *display* only:
//! it never has to be correct enough to execute SQL, just correct enough that nothing is
//! mis-coloured, and it must never lose characters, since the runs are what gets drawn.
//!
//! Parsing for execution belongs to the engine, not here.

/// What a run of characters is, for colouring purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A reserved word.
    Keyword,
    /// An identifier, possibly dotted (`u.id`).
    Identifier,
    /// A single-quoted literal.
    StringLiteral,
    /// A `--` comment, to end of line.
    Comment,
    /// A numeric literal.
    Number,
    /// A `::type` cast suffix.
    Cast,
    /// Anything else printable.
    Punctuation,
    /// A run of spaces or tabs, kept so the line reassembles exactly.
    Whitespace,
}

impl TokenKind {
    /// The tag the view maps onto a colour.
    #[must_use]
    pub const fn as_tag(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::Identifier => "ident",
            Self::StringLiteral => "string",
            Self::Comment => "comment",
            Self::Number => "number",
            Self::Cast => "cast",
            Self::Punctuation => "punct",
            Self::Whitespace => "space",
        }
    }
}

/// One coloured run of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The exact source text of the run.
    pub text: String,
    /// How to colour it.
    pub kind: TokenKind,
}

/// Reserved words, matched case-insensitively against the first dotted segment.
const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ALWAYS",
    "ANALYZE",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "AVG",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CALL",
    "CASCADE",
    "CASE",
    "CAST",
    "CHECK",
    "CHECKPOINT",
    "COALESCE",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONCURRENTLY",
    "CONFLICT",
    "CONSTRAINT",
    "COPY",
    "COUNT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DATABASE",
    "DEFAULT",
    "DEFERRABLE",
    "DELETE",
    "DESC",
    "DISABLE",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "ENABLE",
    "END",
    "EXCEPT",
    "EXECUTE",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FETCH",
    "FILTER",
    "FOLLOWING",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GENERATED",
    "GRANT",
    "GROUP",
    "HAVING",
    "IDENTITY",
    "IF",
    "ILIKE",
    "IN",
    "INCLUDING",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LISTEN",
    "MATERIALIZED",
    "MAX",
    "MIN",
    "NATURAL",
    "NOT",
    "NOTIFY",
    "NULL",
    "NULLS",
    "OF",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "OWNED",
    "PARTITION",
    "POLICY",
    "PRECEDING",
    "PREPARED",
    "PRIMARY",
    "PRIVILEGES",
    "PROCEDURE",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "REFRESH",
    "REINDEX",
    "RELEASE",
    "RENAME",
    "REPLACE",
    "RESTRICT",
    "RETURNING",
    "RETURNS",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SAVEPOINT",
    "SCHEMA",
    "SECURITY",
    "SELECT",
    "SEQUENCE",
    "SET",
    "SHOW",
    "SUM",
    "TABLE",
    "TEMPORARY",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "TRUE",
    "TRUNCATE",
    "TSQUERY",
    "TSVECTOR",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UNLOGGED",
    "UPDATE",
    "USING",
    "VACUUM",
    "VALUES",
    "VECTOR",
    "VERBOSE",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHOUT",
];

/// Whether `word` is a reserved word.
pub fn is_keyword(word: &str) -> bool {
    KEYWORDS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(word))
}

/// Split one line of SQL into coloured runs.
///
/// Concatenating the runs reproduces `line` exactly; the editor relies on that, because the
/// runs are laid out side by side and any dropped character would shift the rest of the line.
#[must_use]
pub fn highlight_line(line: &str) -> Vec<Token> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;

    while at < chars.len() {
        let start = at;
        let kind = match chars[at] {
            // `--` runs to end of line.
            '-' if chars.get(at + 1) == Some(&'-') => {
                at = chars.len();
                TokenKind::Comment
            },
            // A single-quoted literal, in which `''` is an escaped quote. An unterminated
            // literal takes the rest of the line rather than dropping it.
            '\'' => {
                at += 1;
                while at < chars.len() {
                    if chars[at] == '\'' {
                        if chars.get(at + 1) == Some(&'\'') {
                            at += 2;
                            continue;
                        }
                        at += 1;
                        break;
                    }
                    at += 1;
                }
                TokenKind::StringLiteral
            },
            // `::type`, including array and precision suffixes.
            ':' if chars.get(at + 1) == Some(&':') => {
                at += 2;
                while at < chars.len()
                    && (chars[at].is_alphanumeric()
                        || matches!(chars[at], '_' | '[' | ']' | '(' | ')' | ','))
                {
                    at += 1;
                }
                TokenKind::Cast
            },
            c if c.is_ascii_digit() => {
                while at < chars.len() && chars[at].is_ascii_digit() {
                    at += 1;
                }
                if chars.get(at) == Some(&'.')
                    && chars.get(at + 1).is_some_and(char::is_ascii_digit)
                {
                    at += 1;
                    while at < chars.len() && chars[at].is_ascii_digit() {
                        at += 1;
                    }
                }
                TokenKind::Number
            },
            c if c.is_alphabetic() || c == '_' => {
                while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '_') {
                    at += 1;
                }
                // The head of a dotted path decides the colour; the tail is part of the same
                // run so `u.id` is coloured once rather than as three tokens.
                let head: String = chars[start..at].iter().collect();
                while chars.get(at) == Some(&'.')
                    && chars
                        .get(at + 1)
                        .is_some_and(|next| next.is_alphanumeric() || *next == '_')
                {
                    at += 1;
                    while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '_') {
                        at += 1;
                    }
                }
                if is_keyword(&head) {
                    TokenKind::Keyword
                } else {
                    TokenKind::Identifier
                }
            },
            c if c.is_whitespace() => {
                while at < chars.len() && chars[at].is_whitespace() {
                    at += 1;
                }
                TokenKind::Whitespace
            },
            _ => {
                at += 1;
                TokenKind::Punctuation
            },
        };

        tokens.push(Token {
            text: chars[start..at].iter().collect(),
            kind,
        });
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str) -> Vec<TokenKind> {
        highlight_line(line).into_iter().map(|t| t.kind).collect()
    }

    fn texts(line: &str) -> Vec<String> {
        highlight_line(line).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn the_runs_always_reassemble_the_original_line() {
        for line in [
            "SELECT 1;",
            "  SELECT u.id, u.name FROM users u WHERE u.name = 'a''b' -- note",
            "",
            "   ",
            "SUM(o.amount)::numeric(10,4)",
            "unterminated 'string",
            "-- comment only",
        ] {
            assert_eq!(texts(line).concat(), line, "line {line:?} must round-trip");
        }
    }

    #[test]
    fn keywords_are_recognised_regardless_of_case() {
        assert_eq!(kinds("SELECT"), [TokenKind::Keyword]);
        assert_eq!(kinds("select"), [TokenKind::Keyword]);
        assert_eq!(kinds("SeLeCt"), [TokenKind::Keyword]);
        assert_eq!(kinds("selected"), [TokenKind::Identifier]);
    }

    #[test]
    fn a_dotted_path_is_one_run_coloured_by_its_head() {
        assert_eq!(kinds("u.id"), [TokenKind::Identifier]);
        assert_eq!(texts("u.id"), ["u.id"]);
        assert_eq!(texts("a.b.c"), ["a.b.c"]);
    }

    #[test]
    fn a_comment_swallows_the_rest_of_the_line() {
        assert_eq!(
            kinds("SELECT -- not a keyword: SELECT"),
            [
                TokenKind::Keyword,
                TokenKind::Whitespace,
                TokenKind::Comment
            ]
        );
    }

    #[test]
    fn a_doubled_quote_stays_inside_the_literal() {
        assert_eq!(texts("'a''b'"), ["'a''b'"]);
        assert_eq!(kinds("'a''b'"), [TokenKind::StringLiteral]);
    }

    #[test]
    fn casts_numbers_and_punctuation_are_separated() {
        assert_eq!(
            kinds("x::vector(384)"),
            [TokenKind::Identifier, TokenKind::Cast]
        );
        assert_eq!(kinds("3.14"), [TokenKind::Number]);
        assert_eq!(texts("3.14"), ["3.14"]);
        assert_eq!(kinds("1,"), [TokenKind::Number, TokenKind::Punctuation]);
    }

    #[test]
    fn a_minus_that_is_not_a_comment_stays_punctuation() {
        assert_eq!(
            kinds("a - b"),
            [
                TokenKind::Identifier,
                TokenKind::Whitespace,
                TokenKind::Punctuation,
                TokenKind::Whitespace,
                TokenKind::Identifier
            ]
        );
    }
}
