//! Loading a file into a table: CSV / TSV / delimited text or a JSON array of objects, parsed
//! here, matched to the table's columns, and re-encoded as one `COPY … FROM STDIN WITH (FORMAT
//! csv)` stream (`docs/sql-reference.md`, *Bulk load and export*). Columns the file does not
//! have take their defaults, as COPY does.

use crate::backend::quote_ident;

/// How a file's text is split into fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delimiter {
    Comma,
    Semicolon,
    Tab,
    Pipe,
}

impl Delimiter {
    #[must_use]
    pub fn char(self) -> char {
        match self {
            Self::Comma => ',',
            Self::Semicolon => ';',
            Self::Tab => '\t',
            Self::Pipe => '|',
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Comma => ",",
            Self::Semicolon => ";",
            Self::Tab => "tab",
            Self::Pipe => "|",
        }
    }

    pub const ALL: [Self; 4] = [Self::Comma, Self::Semicolon, Self::Tab, Self::Pipe];

    /// The delimiter that splits the first line into the most fields.
    #[must_use]
    pub fn sniff(text: &str) -> Self {
        let first = text.lines().next().unwrap_or("");
        Self::ALL
            .into_iter()
            .max_by_key(|d| first.matches(d.char()).count())
            .filter(|d| first.contains(d.char()))
            .unwrap_or(Self::Comma)
    }
}

/// A parsed file: column names (from the header, or `column1`… without one) and rows, `None`
/// for NULL.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// Whether the names came from the file.
    pub named: bool,
}

/// Split delimited text with RFC 4180 quoting: `"a ""b"""`, quoted delimiters and line breaks.
/// An unquoted empty field is NULL when `empty_is_null`; a quoted one is always an empty string.
#[must_use]
pub fn parse_delimited(
    text: &str,
    delimiter: Delimiter,
    header: bool,
    empty_is_null: bool,
) -> Parsed {
    let d = delimiter.char();
    let mut records: Vec<Vec<Option<String>>> = Vec::new();
    let mut record: Vec<Option<String>> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    let push = |field: &mut String, quoted: &mut bool, record: &mut Vec<Option<String>>| {
        let value = std::mem::take(field);
        record.push(if value.is_empty() && !*quoted && empty_is_null {
            None
        } else {
            Some(value)
        });
        *quoted = false;
    };
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => {
                in_quotes = true;
                quoted = true;
            },
            c if c == d => push(&mut field, &mut quoted, &mut record),
            '\r' => {},
            '\n' => {
                push(&mut field, &mut quoted, &mut record);
                if !(record.len() == 1 && record[0].is_none()) {
                    records.push(std::mem::take(&mut record));
                } else {
                    record.clear();
                }
            },
            c => field.push(c),
        }
    }
    if !field.is_empty() || quoted || !record.is_empty() {
        push(&mut field, &mut quoted, &mut record);
        records.push(record);
    }
    let width = records.iter().map(Vec::len).max().unwrap_or(0);
    let (columns, rows) = if header && !records.is_empty() {
        let names = records.remove(0);
        let mut cols: Vec<String> = names.into_iter().map(Option::unwrap_or_default).collect();
        cols.resize_with(width, String::new);
        (cols, records)
    } else {
        (Vec::new(), records)
    };
    let columns = (0..width)
        .map(|i| {
            columns
                .get(i)
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| format!("column{}", i + 1))
        })
        .collect();
    Parsed {
        columns,
        rows,
        named: header,
    }
}

/// A JSON array of objects: columns in order of first appearance; `null` and missing keys are
/// NULL; numbers, booleans, strings as their text; nested arrays and objects as JSON.
///
/// # Errors
///
/// When the text is not a JSON array of objects.
pub fn parse_json(text: &str) -> Result<Parsed, String> {
    let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("Not valid JSON: {e}"))?;
    let items = value
        .as_array()
        .ok_or("The JSON must be an array of objects, one per row.")?;
    let mut columns: Vec<String> = Vec::new();
    for item in items {
        let object = item
            .as_object()
            .ok_or("Every element of the array must be an object.")?;
        for key in object.keys() {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    let rows = items
        .iter()
        .filter_map(serde_json::Value::as_object)
        .map(|object| {
            columns
                .iter()
                .map(|c| match object.get(c) {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(other) => Some(other.to_string()),
                })
                .collect()
        })
        .collect();
    Ok(Parsed {
        columns,
        rows,
        named: true,
    })
}

/// For each file column, the table column it loads into, if any: by name (case-insensitive)
/// when the file names its columns, by position otherwise.
#[must_use]
pub fn map_columns(parsed: &Parsed, table_columns: &[String]) -> Vec<Option<usize>> {
    parsed
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if parsed.named {
                table_columns
                    .iter()
                    .position(|t| t.eq_ignore_ascii_case(name.trim()))
            } else {
                (i < table_columns.len()).then_some(i)
            }
        })
        .collect()
}

/// The `COPY` statement and its CSV body for the mapped columns. A file column mapped twice is
/// taken once (the first).
///
/// # Errors
///
/// When no file column maps onto the table.
pub fn copy_payload(
    schema: &str,
    table: &str,
    table_columns: &[String],
    parsed: &Parsed,
    mapping: &[Option<usize>],
) -> Result<(String, Vec<u8>), String> {
    let mut used: Vec<(usize, usize)> = Vec::new();
    for (file, target) in mapping.iter().enumerate() {
        if let Some(t) = target
            && !used.iter().any(|(_, u)| u == t)
        {
            used.push((file, *t));
        }
    }
    if used.is_empty() {
        return Err("No column of the file matches a column of the table.".to_owned());
    }
    let names: Vec<String> = used
        .iter()
        .map(|(_, t)| quote_ident(&table_columns[*t]))
        .collect();
    let sql = format!(
        "COPY {}.{} ({}) FROM STDIN WITH (FORMAT csv)",
        quote_ident(schema),
        quote_ident(table),
        names.join(", ")
    );
    let mut body = String::new();
    for row in &parsed.rows {
        let fields: Vec<String> = used
            .iter()
            .map(|(f, _)| match row.get(*f).cloned().flatten() {
                // CSV in COPY: an unquoted empty field is NULL, a quoted one an empty string.
                None => String::new(),
                Some(v) => format!("\"{}\"", v.replace('"', "\"\"")),
            })
            .collect();
        body.push_str(&fields.join(","));
        body.push('\n');
    }
    Ok((sql, body.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quoting_nulls_and_line_breaks_survive() {
        let text =
            "id,name,note\r\n1,\"Budi, Jr.\",\"said \"\"hi\"\"\"\n2,,\"\"\n3,\"two\nlines\",x\n";
        let p = parse_delimited(text, Delimiter::Comma, true, true);
        assert_eq!(p.columns, ["id", "name", "note"]);
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.rows[0][1].as_deref(), Some("Budi, Jr."));
        assert_eq!(p.rows[0][2].as_deref(), Some("said \"hi\""));
        assert_eq!(p.rows[1][1], None);
        assert_eq!(p.rows[1][2].as_deref(), Some(""));
        assert_eq!(p.rows[2][1].as_deref(), Some("two\nlines"));
    }

    #[test]
    fn the_delimiter_is_sniffed_and_a_headerless_file_gets_names() {
        assert_eq!(Delimiter::sniff("a;b;c\n1;2;3"), Delimiter::Semicolon);
        assert_eq!(Delimiter::sniff("a\tb\n"), Delimiter::Tab);
        assert_eq!(Delimiter::sniff("single"), Delimiter::Comma);
        let p = parse_delimited("1|x\n2|y", Delimiter::Pipe, false, true);
        assert_eq!(p.columns, ["column1", "column2"]);
        assert_eq!(
            map_columns(&p, &["id".into(), "name".into(), "extra".into()]),
            [Some(0), Some(1)]
        );
    }

    #[test]
    fn json_rows_become_columns_in_order_of_appearance() {
        let p = parse_json(r#"[{"id":1,"name":"A"},{"id":2,"tags":["x"],"name":null}]"#).unwrap();
        assert_eq!(p.columns, ["id", "name", "tags"]);
        assert_eq!(p.rows[0], [Some("1".into()), Some("A".into()), None]);
        assert_eq!(p.rows[1][2].as_deref(), Some("[\"x\"]"));
        assert!(parse_json("{}").is_err());
    }

    #[test]
    fn the_copy_takes_only_matched_columns_in_file_order() {
        let p = parse_delimited(
            "Name,unknown,ID\nA,zz,1\n,yy,2\n",
            Delimiter::Comma,
            true,
            true,
        );
        let table = ["id".to_owned(), "name".to_owned(), "created".to_owned()];
        let mapping = map_columns(&p, &table);
        assert_eq!(mapping, [Some(1), None, Some(0)]);
        let (sql, body) = copy_payload("public", "t", &table, &p, &mapping).unwrap();
        assert_eq!(
            sql,
            "COPY \"public\".\"t\" (\"name\", \"id\") FROM STDIN WITH (FORMAT csv)"
        );
        assert_eq!(String::from_utf8(body).unwrap(), "\"A\",\"1\"\n,\"2\"\n");
        assert!(copy_payload("public", "t", &table, &p, &[None, None, None]).is_err());
    }
}
