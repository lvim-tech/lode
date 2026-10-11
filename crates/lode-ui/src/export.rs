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

/// One line of CSV or TSV: the column NAMES.
fn header(columns: &[Column], delim: char) -> String {
    columns
        .iter()
        .map(|c| csv_field(&c.name, delim))
        .collect::<Vec<_>>()
        .join(&delim.to_string())
}

/// One line of CSV or TSV: a row's cells, in the columns' order.
///
/// By POSITION against the column list, not by whatever the row happens to
/// hold: a row shorter than the header gets empty cells rather than a line with
/// fewer separators, which would move every column after it.
fn line(columns: &[Column], row: &[Value], delim: char) -> String {
    (0..columns.len())
        .map(|i| {
            row.get(i)
                .and_then(cell_text)
                .map(|s| csv_field(&s, delim))
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(&delim.to_string())
}

/// Write the rendered rows to `path`, creating parent directories.
/// A file being written a page at a time.
///
/// WHY IT IS NOT ONE CALL. `write` renders every row into one string and then
/// writes it: fine for the two hundred rows a page holds, and not fine for a
/// table of two million, where it would want the whole table in memory twice —
/// once as rows and once as text — to produce a file that is written in one go
/// at the end. This holds a `BufWriter` open and takes the rows as they arrive.
///
/// The FORMATS differ in what a file needs around its rows, which is the whole
/// reason this is a type and not a loop: CSV and TSV need a header once, JSON
/// needs brackets and a comma between every pair but not after the last. A
/// writer that forgot the last bracket would produce a file that parses as
/// nothing.
pub struct Streaming {
    out: std::io::BufWriter<std::fs::File>,
    format: Format,
    columns: Vec<Column>,
    written: usize,
    path: std::path::PathBuf,
}

impl Streaming {
    /// Open the file and write whatever comes BEFORE the rows.
    pub fn new(path: &Path, columns: &[Column], format: Format) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = std::fs::File::create(path)?;
        let mut out = std::io::BufWriter::new(file);
        use std::io::Write;
        match format {
            Format::Json => write!(out, "[")?,
            Format::Csv => writeln!(out, "{}", header(columns, ','))?,
            Format::Tsv => writeln!(out, "{}", header(columns, '\t'))?,
        }
        Ok(Streaming {
            out,
            format,
            columns: columns.to_vec(),
            written: 0,
            path: path.to_path_buf(),
        })
    }

    pub fn written(&self) -> usize {
        self.written
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// One page of rows.
    pub fn push(&mut self, rows: &[Vec<Value>]) -> anyhow::Result<()> {
        use std::io::Write;
        for row in rows {
            match self.format {
                Format::Json => {
                    // The comma goes BEFORE every row but the first, so there
                    // is never a trailing one — a file ending in `,]` parses as
                    // nothing, and the alternative is knowing in advance which
                    // row is last, which a stream does not.
                    if self.written > 0 {
                        write!(self.out, ",")?;
                    }
                    let mut m = serde_json::Map::new();
                    for (i, c) in self.columns.iter().enumerate() {
                        m.insert(c.name.clone(), row.get(i).cloned().unwrap_or(Value::Null));
                    }
                    write!(self.out, "{}", Value::Object(m))?;
                }
                Format::Csv => writeln!(self.out, "{}", line(&self.columns, row, ','))?,
                Format::Tsv => writeln!(self.out, "{}", line(&self.columns, row, '\t'))?,
            }
            self.written += 1;
        }
        Ok(())
    }

    /// Write whatever comes AFTER the rows, and flush.
    pub fn finish(mut self) -> anyhow::Result<usize> {
        use std::io::Write;
        if self.format == Format::Json {
            write!(self.out, "]")?;
        }
        self.out.flush()?;
        Ok(self.written)
    }
}

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

    fn named(names: &[&str]) -> Vec<Column> {
        names
            .iter()
            .map(|n| Column { name: n.to_string(), type_name: String::new(), kind: String::new() })
            .collect()
    }

    /// A streamed file is the same file, page by page.
    ///
    /// The thing that goes wrong in a streaming writer is the EDGES — a header
    /// written twice, a trailing comma, a missing bracket — and every one of
    /// them produces a file that parses as nothing while looking roughly right.
    #[test]
    fn a_streamed_file_is_whole() {
        let dir = std::env::temp_dir().join(format!("lode-export-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let c = named(&["a", "b"]);
        let page1 = vec![vec![json!(1), json!("x")], vec![json!(2), json!("y")]];
        let page2 = vec![vec![json!(3), json!("z")]];

        for (fmt, name) in [(Format::Json, "j.json"), (Format::Csv, "c.csv"), (Format::Tsv, "t.tsv")] {
            let path = dir.join(name);
            let mut w = Streaming::new(&path, &c, fmt).expect("opens");
            w.push(&page1).expect("first page");
            w.push(&page2).expect("second page");
            let n = w.finish().expect("closes");
            assert_eq!(n, 3, "{name}");
            let text = std::fs::read_to_string(&path).expect("reads back");
            match fmt {
                Format::Json => {
                    // It PARSES, which is the only thing that matters about a
                    // JSON file, and holds all three rows in order.
                    let v: Value = serde_json::from_str(&text).expect("valid JSON");
                    let arr = v.as_array().expect("an array");
                    assert_eq!(arr.len(), 3);
                    assert_eq!(arr[0]["a"], json!(1));
                    assert_eq!(arr[2]["b"], json!("z"));
                    assert!(!text.contains(",]"), "a trailing comma: {text}");
                }
                _ => {
                    let lines: Vec<&str> = text.lines().collect();
                    assert_eq!(lines.len(), 4, "a header and three rows: {text}");
                    assert_eq!(lines[0].matches(if fmt == Format::Csv { ',' } else { '\t' }).count(), 1);
                    assert!(lines[1].starts_with('1'));
                    assert!(lines[3].starts_with('3'));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing pushed at all still leaves a file that parses.
    #[test]
    fn an_empty_export_is_still_a_file() {
        let dir = std::env::temp_dir().join(format!("lode-export-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let c = named(&["a"]);
        let path = dir.join("e.json");
        let w = Streaming::new(&path, &c, Format::Json).expect("opens");
        assert_eq!(w.finish().expect("closes"), 0);
        let text = std::fs::read_to_string(&path).expect("reads");
        let v: Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(v.as_array().map(Vec::len), Some(0));

        let path = dir.join("e.csv");
        let w = Streaming::new(&path, &c, Format::Csv).expect("opens");
        assert_eq!(w.finish().expect("closes"), 0);
        let text = std::fs::read_to_string(&path).expect("reads");
        assert_eq!(text.lines().count(), 1, "the header, and nothing else");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A row shorter than the header does not move the columns after it.
    #[test]
    fn a_short_row_keeps_its_separators() {
        let c = named(&["a", "b", "c"]);
        assert_eq!(super::line(&c, &[json!(1)], ','), "1,,");
        assert_eq!(super::line(&c, &[], '\t'), "\t\t");
    }

    fn cols() -> Vec<Column> {
        vec![
            Column { name: "id".into(), type_name: "INTEGER".into(), kind: "integer".into() },
            Column { name: "note".into(), type_name: "TEXT".into(), kind: "text".into() },
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
