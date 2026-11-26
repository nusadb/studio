//! Writing results out: CSV, JSON, Excel (.xlsx) and SQL INSERT files, and text for the
//! clipboard.
//!
//! The writer takes rows one at a time, so an export can stream a result of any size straight
//! from the server to disk without holding it. CSV follows RFC 4180 (quotes only where needed,
//! CRLF line ends, UTF-8 with no BOM — what spreadsheet tools and `COPY … CSV` read). JSON is
//! an array of objects that keeps numbers, booleans, nested JSON and NULL typed.

use std::io::{self, Write};

/// File format of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
    /// An Excel workbook: a bold, frozen header row, numbers as numbers, NULL as an empty cell.
    Xlsx,
    /// One `INSERT` per row, into the table named by the file (`customers.sql` → `customers`).
    Sql,
    /// JSON Lines: one object per line, as pandas, Spark and most data tools read it.
    Jsonl,
    /// Tab-separated, header first.
    Tsv,
    /// A Markdown table, for a report, a wiki or an issue.
    Markdown,
}

impl Format {
    /// The format a file name asks for; CSV unless it ends in `.json`, `.xlsx` or `.sql`.
    #[must_use]
    pub fn from_path(path: &std::path::Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("json") => Self::Json,
            Some(ext) if ext.eq_ignore_ascii_case("xlsx") => Self::Xlsx,
            Some(ext) if ext.eq_ignore_ascii_case("sql") => Self::Sql,
            Some(ext)
                if ext.eq_ignore_ascii_case("jsonl") || ext.eq_ignore_ascii_case("ndjson") =>
            {
                Self::Jsonl
            },
            Some(ext) if ext.eq_ignore_ascii_case("tsv") => Self::Tsv,
            Some(ext) if ext.eq_ignore_ascii_case("md") => Self::Markdown,
            _ => Self::Csv,
        }
    }
}

/// Streams rows into `out` in one format.
pub struct Writer<W: Write> {
    out: W,
    format: Format,
    columns: Vec<String>,
    types: Vec<String>,
    rows: usize,
    /// Rows of an Excel export, written out as one workbook at the end.
    held: Vec<Vec<Option<String>>>,
    /// Table an SQL export inserts into, already quoted.
    table: String,
}

/// Most data rows an Excel sheet holds (the header takes one more).
const XLSX_MAX_ROWS: usize = 1_048_575;

/// The table an SQL export inserts into: the file's name, quoted when it has to be.
#[must_use]
pub fn table_from_path(path: &std::path::Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("result");
    crate::backend::quote_ident(stem)
}

impl<W: Write> Writer<W> {
    /// Start an export of `columns` (with their SQL `types`), writing the header if the format
    /// has one.
    ///
    /// # Errors
    ///
    /// The I/O error from writing the header.
    pub fn new(
        mut out: W,
        format: Format,
        columns: Vec<String>,
        types: Vec<String>,
    ) -> io::Result<Self> {
        match format {
            Format::Csv => {
                let header: Vec<String> = columns.iter().map(|c| csv_field(c)).collect();
                write!(out, "{}\r\n", header.join(","))?;
            },
            Format::Json => out.write_all(b"[")?,
            Format::Sql => {
                writeln!(out, "-- Exported by NusaDB Studio")?;
            },
            Format::Tsv => writeln!(
                out,
                "{}",
                columns
                    .iter()
                    .map(|c| tsv_field(c))
                    .collect::<Vec<_>>()
                    .join("\t")
            )?,
            Format::Markdown => {
                writeln!(
                    out,
                    "| {} |",
                    columns
                        .iter()
                        .map(|c| md_field(c))
                        .collect::<Vec<_>>()
                        .join(" | ")
                )?;
                writeln!(out, "|{}", " --- |".repeat(columns.len()))?;
            },
            Format::Xlsx | Format::Jsonl => {},
        }
        Ok(Self {
            out,
            format,
            columns,
            types,
            rows: 0,
            held: Vec::new(),
            table: "\"result\"".to_owned(),
        })
    }

    /// Name the table an SQL export inserts into (already quoted).
    #[must_use]
    pub fn with_table(mut self, table: String) -> Self {
        self.table = table;
        self
    }

    /// Append one row. `None` is SQL NULL.
    ///
    /// # Errors
    ///
    /// The I/O error from writing.
    pub fn row(&mut self, values: &[Option<String>]) -> io::Result<()> {
        match self.format {
            Format::Csv => {
                // NULL is an empty unquoted field; an empty string is `""`, so the two survive
                // a round trip through COPY.
                let fields: Vec<String> = values
                    .iter()
                    .map(|v| match v {
                        None => String::new(),
                        Some(text) if text.is_empty() => "\"\"".to_owned(),
                        Some(text) => csv_field(text),
                    })
                    .collect();
                write!(self.out, "{}\r\n", fields.join(","))?;
            },
            Format::Jsonl => {
                let mut object = serde_json::Map::new();
                for (i, name) in self.columns.iter().enumerate() {
                    let kind = self.types.get(i).map_or("", String::as_str);
                    object.insert(
                        name.clone(),
                        json_value(values.get(i).cloned().flatten(), kind),
                    );
                }
                serde_json::to_writer(&mut self.out, &serde_json::Value::Object(object))
                    .map_err(io::Error::other)?;
                self.out.write_all(b"\n")?;
            },
            Format::Tsv => {
                let fields: Vec<String> = values
                    .iter()
                    .map(|v| v.as_deref().map(tsv_field).unwrap_or_default())
                    .collect();
                writeln!(self.out, "{}", fields.join("\t"))?;
            },
            Format::Markdown => {
                let fields: Vec<String> = values
                    .iter()
                    .map(|v| v.as_deref().map_or_else(|| "NULL".to_owned(), md_field))
                    .collect();
                writeln!(self.out, "| {} |", fields.join(" | "))?;
            },
            Format::Json => {
                let mut object = serde_json::Map::new();
                for (i, name) in self.columns.iter().enumerate() {
                    let kind = self.types.get(i).map_or("", String::as_str);
                    object.insert(
                        name.clone(),
                        json_value(values.get(i).cloned().flatten(), kind),
                    );
                }
                let separator: &[u8] = if self.rows == 0 { b"\n  " } else { b",\n  " };
                self.out.write_all(separator)?;
                serde_json::to_writer(&mut self.out, &serde_json::Value::Object(object))
                    .map_err(io::Error::other)?;
            },
            Format::Sql => {
                let cols = self
                    .columns
                    .iter()
                    .map(|c| crate::backend::quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                let values = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let kind = self.types.get(i).map_or("", String::as_str);
                        sql_literal(v.as_deref(), kind)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    self.out,
                    "INSERT INTO {} ({cols}) VALUES ({values});",
                    self.table
                )?;
            },
            Format::Xlsx => {
                if self.held.len() >= XLSX_MAX_ROWS {
                    return Err(io::Error::other(format!(
                        "an Excel sheet holds at most {XLSX_MAX_ROWS} rows; export to CSV instead"
                    )));
                }
                self.held.push(values.to_vec());
            },
        }
        self.rows += 1;
        Ok(())
    }

    /// Close the export and return how many rows it holds.
    ///
    /// # Errors
    ///
    /// The I/O error from writing the trailer or flushing.
    pub fn finish(mut self) -> io::Result<usize> {
        if self.format == Format::Xlsx {
            let bytes = xlsx(&self.columns, &self.types, &self.held).map_err(io::Error::other)?;
            self.out.write_all(&bytes)?;
        }
        if self.format == Format::Json {
            self.out
                .write_all(if self.rows == 0 { b"]\n" } else { b"\n]\n" })?;
        }
        self.out.flush()?;
        Ok(self.rows)
    }
}

/// A TSV field: tabs and line breaks would split it, so they become spaces.
fn tsv_field(v: &str) -> String {
    v.replace(['\t', '\n', '\r'], " ")
}

/// A Markdown table cell: a pipe would end the cell and a line break the row.
fn md_field(v: &str) -> String {
    v.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Whether a column type is written as a number (Excel) or bare (SQL).
fn numeric(kind: &str) -> bool {
    matches!(kind, "int" | "float" | "numeric")
}

/// A value as an SQL literal: NULL, a bare number, or a quoted string.
fn sql_literal(value: Option<&str>, kind: &str) -> String {
    match value {
        None => "NULL".to_owned(),
        Some(v) if numeric(kind) && !v.is_empty() && v.parse::<f64>().is_ok() => v.to_owned(),
        Some(v) => format!("'{}'", v.replace('\'', "''")),
    }
}

/// An Excel workbook of one sheet holding `rows`.
fn xlsx(
    columns: &[String],
    types: &[String],
    rows: &[Vec<Option<String>>],
) -> Result<Vec<u8>, rust_xlsxwriter::XlsxError> {
    // A cell holds at most this many characters.
    const CELL_MAX: usize = 32_767;
    let mut book = rust_xlsxwriter::Workbook::new();
    let bold = rust_xlsxwriter::Format::new().set_bold();
    let sheet = book.add_worksheet();
    sheet.set_name("Export")?;
    for (c, name) in columns.iter().enumerate() {
        let col = u16::try_from(c).unwrap_or(u16::MAX);
        sheet.write_string_with_format(0, col, name, &bold)?;
    }
    sheet.set_freeze_panes(1, 0)?;
    for (r, row) in rows.iter().enumerate() {
        let at = u32::try_from(r + 1).unwrap_or(u32::MAX);
        for (c, value) in row.iter().enumerate() {
            let col = u16::try_from(c).unwrap_or(u16::MAX);
            let Some(v) = value else { continue };
            let kind = types.get(c).map_or("", String::as_str);
            match v.parse::<f64>() {
                Ok(n) if numeric(kind) && n.is_finite() => {
                    sheet.write_number(at, col, n)?;
                },
                _ => {
                    let text: String = v.chars().take(CELL_MAX).collect();
                    sheet.write_string(at, col, text)?;
                },
            }
        }
    }
    sheet.autofit();
    book.save_to_buffer()
}

/// One CSV field, quoted only when it has to be.
fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) || text.starts_with(' ') || text.ends_with(' ') {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

/// A cell as typed JSON.
#[must_use]
pub fn json_value(value: Option<String>, kind: &str) -> serde_json::Value {
    use serde_json::Value;
    match value {
        None => Value::Null,
        Some(text) if matches!(kind, "int" | "float" | "numeric") => text
            .parse::<serde_json::Number>()
            .map_or(Value::String(text), Value::Number),
        Some(text) if kind == "bool" => match text.as_str() {
            "true" | "t" => Value::Bool(true),
            "false" | "f" => Value::Bool(false),
            _ => Value::String(text),
        },
        Some(text) if kind == "json" => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        Some(text) => Value::String(text),
    }
}

/// Rows as tab-separated text with a header line — what a spreadsheet expects on paste.
#[must_use]
pub fn tsv(columns: &[String], rows: &[Vec<Option<String>>]) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = columns
        .iter()
        .map(|c| clean(c))
        .collect::<Vec<_>>()
        .join("\t");
    for row in rows {
        out.push('\n');
        let cells: Vec<String> = row
            .iter()
            .map(|v| v.as_deref().map_or_else(|| "NULL".to_owned(), clean))
            .collect();
        out.push_str(&cells.join("\t"));
    }
    out
}

/// A whole result as text in `format`, for the clipboard.
#[must_use]
#[cfg(test)]
pub fn to_text(
    format: Format,
    columns: &[String],
    types: &[String],
    rows: &[Vec<Option<String>>],
) -> String {
    String::from_utf8(to_bytes(
        format,
        columns,
        types,
        rows,
        "\"result\"".to_owned(),
    ))
    .unwrap_or_default()
}

/// A whole result as a file's bytes in `format` (an `.xlsx` is binary).
#[must_use]
pub fn to_bytes(
    format: Format,
    columns: &[String],
    types: &[String],
    rows: &[Vec<Option<String>>],
    table: String,
) -> Vec<u8> {
    let mut buf = Vec::new();
    let written = Writer::new(&mut buf, format, columns.to_vec(), types.to_vec()).and_then(|w| {
        let mut w = w.with_table(table);
        for row in rows {
            w.row(row)?;
        }
        w.finish()
    });
    if written.is_err() {
        return Vec::new();
    }
    buf
}

#[cfg(test)]
mod tests {
    #[test]
    fn jsonl_tsv_and_markdown() {
        for (ext, expect) in [
            ("jsonl", "{\"id\":1,\"name\":\"a|b\"}\n"),
            ("tsv", "id\tname\n1\ta|b\n"),
            ("md", "| id | name |\n| --- | --- |\n| 1 | a\\|b |\n"),
        ] {
            let mut out = Vec::new();
            let mut w = Writer::new(
                &mut out,
                Format::from_path(std::path::Path::new(&format!("x.{ext}"))),
                vec!["id".into(), "name".into()],
                vec!["int".into(), "text".into()],
            )
            .unwrap();
            w.row(&[Some("1".into()), Some("a|b".into())]).unwrap();
            w.finish().unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), expect, "{ext}");
        }
    }

    use super::*;

    #[test]
    fn sql_exports_insert_into_the_file_name() {
        let (columns, types, rows) = sample();
        let table = table_from_path(std::path::Path::new("/tmp/Order Items.sql"));
        let text =
            String::from_utf8(to_bytes(Format::Sql, &columns, &types, &rows, table)).unwrap();
        assert!(text.starts_with("-- Exported by NusaDB Studio\n"));
        assert!(text.contains("INSERT INTO \"Order Items\" ("), "{text}");
        assert!(text.contains("NULL"), "{text}");
        assert_eq!(
            Format::from_path(std::path::Path::new("a.SQL")),
            Format::Sql
        );
    }

    #[test]
    fn excel_exports_are_a_real_workbook() {
        let (columns, types, rows) = sample();
        let bytes = to_bytes(Format::Xlsx, &columns, &types, &rows, String::new());
        // An .xlsx is a zip archive.
        assert_eq!(&bytes[..2], b"PK");
        assert!(bytes.len() > 1000);
        assert_eq!(
            Format::from_path(std::path::Path::new("x.xlsx")),
            Format::Xlsx
        );
    }

    fn sample() -> (Vec<String>, Vec<String>, Vec<Vec<Option<String>>>) {
        (
            vec!["id".into(), "note".into(), "ok".into()],
            vec!["int".into(), "text".into(), "bool".into()],
            vec![
                vec![
                    Some("1".into()),
                    Some("a, \"b\"\nc".into()),
                    Some("true".into()),
                ],
                vec![Some("2".into()), None, Some("false".into())],
                vec![Some("3".into()), Some(String::new()), None],
            ],
        )
    }

    #[test]
    fn csv_quotes_only_what_needs_it_and_keeps_null_apart_from_empty() {
        let (c, t, r) = sample();
        let text = to_text(Format::Csv, &c, &t, &r);
        assert_eq!(
            text,
            "id,note,ok\r\n1,\"a, \"\"b\"\"\nc\",true\r\n2,,false\r\n3,\"\",\r\n"
        );
    }

    #[test]
    fn json_is_a_typed_array() {
        let (c, t, r) = sample();
        let parsed: serde_json::Value =
            serde_json::from_str(&to_text(Format::Json, &c, &t, &r)).unwrap();
        assert_eq!(parsed[0]["id"], 1);
        assert_eq!(parsed[0]["ok"], true);
        assert!(parsed[1]["note"].is_null());
        assert_eq!(parsed[2]["note"], "");
        assert_eq!(parsed.as_array().map(Vec::len), Some(3));
        let empty: serde_json::Value =
            serde_json::from_str(&to_text(Format::Json, &c, &t, &[])).unwrap();
        assert_eq!(empty, serde_json::json!([]));
    }

    #[test]
    fn tsv_pastes_into_a_spreadsheet() {
        let (c, _, r) = sample();
        let text = tsv(&c, &r[..2]);
        assert_eq!(text, "id\tnote\tok\n1\ta, \"b\" c\ttrue\n2\tNULL\tfalse");
    }

    #[test]
    fn the_extension_picks_the_format() {
        assert_eq!(
            Format::from_path(std::path::Path::new("a.JSON")),
            Format::Json
        );
        assert_eq!(
            Format::from_path(std::path::Path::new("a.csv")),
            Format::Csv
        );
        assert_eq!(Format::from_path(std::path::Path::new("a")), Format::Csv);
    }
}
