//! Export a loaded page.
//!
//! What is exported is what is LOADED — the rows the grid actually has — and the
//! UI says so. An export that silently paged the whole table in the background
//! would be a different feature with a different cost, and offering it under the
//! same button would mean the user could not tell which one they got.

use std::fmt::Write as _;
use std::path::Path;

use lode_proto::{cell_text, Column};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    Json,
}

impl Format {
    pub fn label(&self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
            Format::Json => "JSON",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
        }
    }
}

/// RFC 4180 quoting: a field is quoted when it holds the delimiter, a quote or a
/// newline, and an inner quote is doubled. Without this a cell containing a
/// comma turns one row into two columns and the file is quietly wrong.
fn csv_field(s: &str, delim: char) -> String {
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Render the rows. A SQL NULL is an EMPTY field in CSV/TSV and a JSON `null` —
/// never the text "NULL", which is a value a column can legitimately hold.
pub fn render(columns: &[Column], rows: &[Vec<Value>], format: Format) -> String {
    match format {
        Format::Json => {
            let objs: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let mut m = serde_json::Map::new();
                    for (i, c) in columns.iter().enumerate() {
                        m.insert(c.name.clone(), r.get(i).cloned().unwrap_or(Value::Null));
                    }
                    Value::Object(m)
                })
                .collect();
            serde_json::to_string_pretty(&objs).unwrap_or_else(|_| "[]".into())
        }
        Format::Csv | Format::Tsv => {
            let delim = if format == Format::Csv { ',' } else { '\t' };
            let mut out = String::new();
            let names: Vec<String> = columns.iter().map(|c| csv_field(&c.name, delim)).collect();
            let _ = writeln!(out, "{}", names.join(&delim.to_string()));
            for r in rows {
                let cells: Vec<String> = (0..columns.len())
                    .map(|i| {
                        r.get(i)
                            .and_then(cell_text)
                            .map(|s| csv_field(&s, delim))
                            .unwrap_or_default()
                    })
                    .collect();
                let _ = writeln!(out, "{}", cells.join(&delim.to_string()));
            }
            out
        }
    }
}

/// Write the rendered rows to `path`, creating parent directories.
pub fn write(path: &Path, columns: &[Column], rows: &[Vec<Value>], format: Format) -> anyhow::Result<usize> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let body = render(columns, rows, format);
    std::fs::write(path, body.as_bytes())?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cols() -> Vec<Column> {
        vec![
            Column { name: "id".into(), type_name: "INTEGER".into() },
            Column { name: "note".into(), type_name: "TEXT".into() },
        ]
    }

    #[test]
    fn csv_quotes_what_it_must_and_leaves_null_empty() {
        let rows = vec![
            vec![json!(1), json!("plain")],
            vec![json!(2), json!("has, comma")],
            vec![json!(3), json!("has \"quote\"")],
            vec![json!(4), Value::Null],
            // A string that READS like NULL must survive as that string.
            vec![json!(5), json!("NULL")],
        ];
        let out = render(&cols(), &rows, Format::Csv);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "id,note");
        assert_eq!(lines[1], "1,plain");
        assert_eq!(lines[2], "2,\"has, comma\"");
        assert_eq!(lines[3], "3,\"has \"\"quote\"\"\"");
        assert_eq!(lines[4], "4,", "a NULL is an empty field");
        assert_eq!(lines[5], "5,NULL", "the STRING \"NULL\" is not a null");
    }

    #[test]
    fn tsv_uses_tabs() {
        let rows = vec![vec![json!(1), json!("a,b")]];
        let out = render(&cols(), &rows, Format::Tsv);
        assert_eq!(out.lines().nth(1), Some("1\ta,b"), "a comma needs no quoting in TSV");
    }

    #[test]
    fn json_keeps_null_as_null() {
        let rows = vec![vec![json!(1), Value::Null]];
        let out = render(&cols(), &rows, Format::Json);
        let back: Vec<serde_json::Map<String, Value>> = serde_json::from_str(&out).unwrap();
        assert_eq!(back[0]["id"], json!(1));
        assert_eq!(back[0]["note"], Value::Null);
    }

    #[test]
    fn a_short_row_pads_rather_than_shifting() {
        // A driver that returns fewer cells than columns must not make the file
        // misaligned; the missing cell is empty, in its own position.
        let rows = vec![vec![json!(1)]];
        let out = render(&cols(), &rows, Format::Csv);
        assert_eq!(out.lines().nth(1), Some("1,"));
    }
}
