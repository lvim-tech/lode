// lode/drivers/influxdb: time series, v1 and v2.
//
// NOT VERIFIED AGAINST A REAL INFLUXDB of either version. No network here, no
// server. Tested properly: which version a connection is, the request each
// version takes, the two completely different answer formats read into the same
// grid, and which statements change something. Untested: the HTTP round trip.
//
// TWO VERSIONS, TWO LANGUAGES, AND THEY ARE NOT ALIKE. v1 speaks InfluxQL —
// SQL-shaped, `SELECT … FROM … WHERE time > …` — over `/query`, authenticated
// with a username and password, and answers with a nest of series. v2 speaks
// Flux — a pipeline language that looks nothing like SQL — over
// `/api/v2/query`, authenticated with a token, and answers with annotated CSV.
//
// One driver with a `version` field rather than two drivers, because everything
// else about them is the same connection: a host, an organisation, a bucket or
// a database. What differs is declared per version, which is why the hint and
// the danger rules below ask which one they are for.

use async_trait::async_trait;

use crate::driver::{Connection, Driver};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, Caps, Column, ConnSpec, DriverMeta, ParamSpec, ParamType, Value,
};

const PARAMS: &[ParamSpec] = &[
    ParamSpec { key: "url", label: "URL", kind: ParamType::String, required: true, secret: false, default: Some("http://localhost:8086") },
    ParamSpec { key: "version", label: "Version (1 or 2)", kind: ParamType::Enum, required: true, secret: false, default: Some("2") },
    // v2.
    ParamSpec { key: "org", label: "Organisation (v2)", kind: ParamType::String, required: false, secret: false, default: None },
    ParamSpec { key: "bucket", label: "Bucket (v2)", kind: ParamType::String, required: false, secret: false, default: None },
    ParamSpec { key: "token", label: "Token (v2)", kind: ParamType::String, required: false, secret: true, default: None },
    // v1.
    ParamSpec { key: "database", label: "Database (v1)", kind: ParamType::String, required: false, secret: false, default: None },
    ParamSpec { key: "user", label: "User (v1)", kind: ParamType::String, required: false, secret: false, default: None },
    ParamSpec { key: "password", label: "Password (v1)", kind: ParamType::String, required: false, secret: true, default: None },
];

const META: DriverMeta = DriverMeta {
    kind: "influxdb",
    display: "InfluxDB (v1 and v2)",
    default_port: Some(8086),
    params: PARAMS,
    auth: &[AuthKind::None, AuthKind::Password],
    caps: Caps {
        // v1 is SQL-SHAPED and v2 is not, and one flag cannot be both. False,
        // because the flag decides whether a client offers SQL-ish helpers,
        // and offering them for Flux would be offering the wrong thing to half
        // the connections.
        sql: false,
        schemas: true,
        cancel: false,
        tls: true,
        tunnel: true,
        multi_db: false,
        kv: false,
        indexes: false,
        ddl: false,
        metrics: false,
        edit: false,
    },
    hint: "from(bucket: \"b\") |> range(start: -1h)",
    browse: "from(bucket: \"{schema}\") |> range(start: -1h) |> filter(fn: (r) => r._measurement == \"{name}\")",
    browse_by_kind: &[],
    query_form: &[],
    query_template: "",
    browse_form: &[],
    // Flux's own range call. `{col}` is unused: in InfluxDB the time column is
    // `_time` and there is no choosing it, which the template says by not
    // taking one.
    time_filter: "|> range(start: {from}, stop: {to})",
    builder_ops: &[],
    inspectors: &[],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V1,
    V2,
}

/// Which version a spec is, with the fields that version needs checked.
///
/// Checked here rather than at the first request, so "you chose v2 and gave no
/// token" is said while somebody is looking at the form instead of as a 401
/// afterwards.
pub fn version_of(spec: &ConnSpec) -> anyhow::Result<Version> {
    let raw = spec.param_opt("version").unwrap_or("2").trim().to_string();
    let v = match raw.as_str() {
        "1" | "v1" => Version::V1,
        "2" | "v2" => Version::V2,
        other => anyhow::bail!("'{other}' is not an InfluxDB version — there are 1 and 2"),
    };
    match v {
        Version::V1 if spec.param_opt("database").is_none() => {
            anyhow::bail!("InfluxDB v1 needs a database")
        }
        Version::V2 if spec.param_opt("org").is_none() => {
            anyhow::bail!("InfluxDB v2 needs an organisation")
        }
        _ => Ok(v),
    }
}

/// What an empty editor should offer, per version — because the two languages
/// are not alike and one example would be wrong for half the connections.
pub fn hint_for(v: Version) -> &'static str {
    match v {
        Version::V1 => "SELECT * FROM measurement WHERE time > now() - 1h LIMIT 100",
        Version::V2 => "from(bucket: \"b\") |> range(start: -1h) |> limit(n: 100)",
    }
}

/// The path, the query and the body a statement becomes.
pub fn request_for(v: Version, statement: &str, db_or_org: &str) -> (String, Vec<(String, String)>, String, &'static str) {
    match v {
        Version::V1 => (
            "/query".into(),
            vec![
                ("db".into(), db_or_org.to_string()),
                ("q".into(), statement.to_string()),
                // Without this, v1 returns times as nanoseconds since the epoch
                // in a float, which loses precision and is unreadable anyway.
                ("epoch".into(), "ms".into()),
            ],
            String::new(),
            "application/x-www-form-urlencoded",
        ),
        Version::V2 => (
            "/api/v2/query".into(),
            vec![("org".into(), db_or_org.to_string())],
            statement.to_string(),
            "application/vnd.flux",
        ),
    }
}

/// v1's answer: a nest of results → series → columns + values.
///
/// Flattened into one grid, with the series' tags folded in as columns, because
/// two measurements in one answer are still one table to read — and a client
/// that showed only the first series would silently drop the rest.
pub fn parse_v1(v: &serde_json::Value) -> anyhow::Result<(Vec<Column>, Vec<Vec<Value>>)> {
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("{err}");
    }
    let results = v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    // An error can also arrive inside a result, which is easy to miss and shows
    // as an empty grid instead of a message.
    for r in &results {
        if let Some(err) = r.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("{err}");
        }
    }
    let mut names: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut staged: Vec<(Vec<String>, Vec<Value>)> = Vec::new();
    for r in &results {
        for s in r.get("series").and_then(|s| s.as_array()).cloned().unwrap_or_default() {
            let measurement = s.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
            let tags = s.get("tags").and_then(|t| t.as_object()).cloned().unwrap_or_default();
            let cols: Vec<String> = s
                .get("columns")
                .and_then(|c| c.as_array())
                .map(|a| a.iter().map(|c| c.as_str().unwrap_or("").to_string()).collect())
                .unwrap_or_default();
            let mut header: Vec<String> = vec!["measurement".into()];
            header.extend(tags.keys().cloned());
            header.extend(cols.iter().cloned());
            for n in &header {
                if !names.contains(n) {
                    names.push(n.clone());
                }
            }
            for row in s.get("values").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                let mut cells: Vec<Value> = vec![Value::Text(measurement.clone())];
                for t in tags.values() {
                    cells.push(Value::Text(t.as_str().unwrap_or("").to_string()));
                }
                for c in row.as_array().cloned().unwrap_or_default() {
                    cells.push(crate::spec::Value::from_json(c));
                }
                staged.push((header.clone(), cells));
            }
        }
    }
    // Lined up by NAME, so two series with different columns do not shift each
    // other's values into the wrong column.
    for (header, cells) in staged {
        let mut row = vec![Value::Null; names.len()];
        for (h, c) in header.iter().zip(cells) {
            if let Some(i) = names.iter().position(|n| n == h) {
                row[i] = c;
            }
        }
        rows.push(row);
    }
    let columns = names
        .iter()
        .map(|n| Column { name: n.clone(), type_name: String::new() })
        .collect();
    Ok((columns, rows))
}

/// v2's answer: annotated CSV.
///
/// The annotation lines start with `#` and carry the datatypes; the first
/// non-annotation line is the header. The leading empty column is InfluxDB's
/// own and is dropped — kept, every grid would start with a blank column.
pub fn parse_v2_csv(csv: &str) -> (Vec<Column>, Vec<Vec<Value>>) {
    // The annotation has ONE FEWER entry than a data line, because every line
    // starts with InfluxDB's own empty column and `#datatype` does not count
    // it. Dropping that column FIRST, from the header and from every row,
    // leaves the two aligned — the first version typed each raw field by
    // `datatypes[i]` and was off by one the whole way across, so `table` was
    // read as a date and `_time` as a number.
    let mut datatypes: Vec<String> = Vec::new();
    let mut header: Vec<String> = Vec::new();
    let mut raw_rows: Vec<Vec<String>> = Vec::new();
    for line in csv.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#datatype,") {
            datatypes = rest.split(',').map(|s| s.trim().to_string()).collect();
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let cells: Vec<String> = line.split(',').map(|c| c.trim().to_string()).collect();
        if header.is_empty() {
            header = cells;
            continue;
        }
        raw_rows.push(cells);
    }
    // InfluxDB's own leading column, taken off the header and the rows together.
    let drop_first = header.first().map(|h| h.is_empty()).unwrap_or(false);
    if drop_first {
        header.remove(0);
        for r in raw_rows.iter_mut() {
            if !r.is_empty() {
                r.remove(0);
            }
        }
    }
    let rows: Vec<Vec<Value>> = raw_rows
        .into_iter()
        .map(|cells| {
            cells
                .iter()
                .enumerate()
                .map(|(i, c)| typed(c, datatypes.get(i).map(String::as_str).unwrap_or("string")))
                .collect()
        })
        .collect();
    let columns = header
        .iter()
        .enumerate()
        .map(|(i, n)| Column {
            name: n.clone(),
            type_name: datatypes.get(i).cloned().unwrap_or_default(),
        })
        .collect();
    (columns, rows)
}

/// One CSV cell, by the datatype the annotation gave it.
fn typed(raw: &str, datatype: &str) -> Value {
    if raw.is_empty() {
        return Value::Null;
    }
    match datatype {
        "long" | "unsignedLong" => raw.parse::<i64>().map(Value::Int).unwrap_or_else(|_| Value::Text(raw.into())),
        "double" => raw.parse::<f64>().map(Value::Float).unwrap_or_else(|_| Value::Text(raw.into())),
        "boolean" => Value::Bool(raw == "true"),
        "dateTime:RFC3339" => Value::Timestamp(raw.to_string()),
        _ => Value::Text(raw.to_string()),
    }
}

/// Does this statement change the database?
///
/// Per version, because the two languages are not alike. InfluxQL has DROP and
/// DELETE; Flux has `to()`, which writes a result back into a bucket and is the
/// only way a read turns into a write there. Missing `to()` would let a Flux
/// script write through a read-only connection.
pub fn writes_in(statement: &str) -> bool {
    let s = statement.to_ascii_lowercase();
    if is_flux(&s) {
        // In Flux, `to()` is the ONLY thing that writes. Falling through to the
        // SQL rule here was a real bug: that rule treats an unrecognised
        // leading verb as a write, and every Flux script begins `from(` — so a
        // read-only connection refused every read. Found by the test that
        // asserts a plain `from(...) |> range(...)` is not a write.
        return s.contains("|> to(") || s.contains("|>to(");
    }
    crate::driver::default_sql_writes(statement)
}

/// Is this Flux rather than InfluxQL?
///
/// By SHAPE, because the two languages share no keywords: Flux is a pipeline of
/// calls and says so with `|>`, or it begins with `import` or `from(`. Guessing
/// from the connection's version would be worse — somebody pasting InfluxQL
/// into a v2 connection should be told what is wrong with the statement, not
/// have it misjudged.
pub fn is_flux(lowered: &str) -> bool {
    let t = lowered.trim_start();
    t.contains("|>") || t.starts_with("from(") || t.starts_with("import ")
}

/// Why this is destructive, in either language.
pub fn classify_in(statement: &str) -> Option<String> {
    let s = statement.to_ascii_lowercase();
    if is_flux(&s) {
        if s.contains("|> to(") || s.contains("|>to(") {
            return Some(
                "this Flux script ends in to(), which WRITES its result into a bucket".to_string(),
            );
        }
        // Nothing else in Flux changes anything, and running the SQL rule over
        // it would find words inside string literals.
        return None;
    }
    // InfluxQL's own destructive verbs, which the SQL rule does not know.
    for (word, why) in [
        ("drop measurement", "DROP MEASUREMENT removes a measurement and every point in it"),
        ("drop database", "DROP DATABASE removes a database and everything in it"),
        ("drop series", "DROP SERIES removes every point in the matching series"),
        ("delete from", "DELETE removes points, and with no WHERE it removes all of them"),
    ] {
        if s.contains(word) {
            return Some(why.to_string());
        }
    }
    crate::driver::default_sql_classify(statement)
}

pub struct InfluxDriver;

impl InfluxDriver {
    pub fn new() -> Self {
        InfluxDriver
    }
}

#[async_trait]
impl Driver for InfluxDriver {
    fn meta(&self) -> &'static DriverMeta {
        &META
    }

    async fn connect(&self, spec: &ConnSpec, _net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        // Checked even here, so a spec that could never work says why rather
        // than reaching the "not verified" message and looking like the same
        // problem as a correct one.
        let v = version_of(spec)?;
        Err(anyhow::anyhow!(
            "the InfluxDB driver (v{}) is written against the documented API but is NOT VERIFIED \
             — no network here and no server. Its request building and its two answer formats are \
             tested; the round trip between them is not.",
            if v == Version::V1 { 1 } else { 2 }
        ))
    }

    fn classify(&self, statement: &str) -> Option<String> {
        classify_in(statement)
    }

    fn writes(&self, statement: &str) -> bool {
        writes_in(statement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(params: serde_json::Value) -> ConnSpec {
        serde_json::from_value(json!({ "driver": "influxdb", "params": params })).unwrap()
    }

    /// Said while somebody is looking at the form, not as a 401 afterwards.
    #[test]
    fn a_version_brings_the_fields_that_version_needs() {
        assert_eq!(version_of(&spec(json!({ "version": "2", "org": "acme" }))).unwrap(), Version::V2);
        assert_eq!(version_of(&spec(json!({ "version": "1", "database": "metrics" }))).unwrap(), Version::V1);
        let e = version_of(&spec(json!({ "version": "2" }))).unwrap_err().to_string();
        assert!(e.contains("organisation"), "{e}");
        let e = version_of(&spec(json!({ "version": "1" }))).unwrap_err().to_string();
        assert!(e.contains("database"), "{e}");
        let e = version_of(&spec(json!({ "version": "3" }))).unwrap_err().to_string();
        assert!(e.contains("there are 1 and 2"), "{e}");
    }

    /// Two languages, two examples. One hint would be wrong for half the
    /// connections.
    #[test]
    fn each_version_suggests_its_own_language() {
        assert!(hint_for(Version::V1).starts_with("SELECT"));
        assert!(hint_for(Version::V2).starts_with("from(bucket"));
    }

    #[test]
    fn the_two_versions_take_completely_different_requests() {
        let (path, q, body, ctype) = request_for(Version::V1, "SELECT * FROM cpu", "metrics");
        assert_eq!(path, "/query");
        assert!(q.contains(&("db".into(), "metrics".into())));
        assert!(q.contains(&("q".into(), "SELECT * FROM cpu".into())));
        // Without this, v1 returns nanosecond floats.
        assert!(q.contains(&("epoch".into(), "ms".into())));
        assert!(body.is_empty());
        assert_eq!(ctype, "application/x-www-form-urlencoded");

        let (path, q, body, ctype) = request_for(Version::V2, "from(bucket:\"b\")", "acme");
        assert_eq!(path, "/api/v2/query");
        assert!(q.contains(&("org".into(), "acme".into())));
        // Flux goes in the BODY, not the query string.
        assert_eq!(body, "from(bucket:\"b\")");
        assert_eq!(ctype, "application/vnd.flux");
    }

    /// Two series in one answer are still one table, and a client showing only
    /// the first would silently drop the rest.
    #[test]
    fn a_v1_answer_flattens_its_series_without_losing_any() {
        let v = json!({ "results": [{ "series": [
            { "name": "cpu", "tags": { "host": "a" }, "columns": ["time", "value"],
              "values": [[1767225600000i64, 0.5], [1767225601000i64, 0.6]] },
            { "name": "mem", "tags": { "host": "b" }, "columns": ["time", "used"],
              "values": [[1767225600000i64, 1024]] }
        ]}]});
        let (cols, rows) = parse_v1(&v).unwrap();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"measurement") && names.contains(&"host"));
        assert!(names.contains(&"value") && names.contains(&"used"));
        assert_eq!(rows.len(), 3, "every point from both series");
        // Lined up by NAME: `used` must not land in `value`'s column.
        let used_at = names.iter().position(|n| *n == "used").unwrap();
        let value_at = names.iter().position(|n| *n == "value").unwrap();
        assert!(matches!(rows[2][used_at], Value::Int(1024)));
        assert!(matches!(rows[2][value_at], Value::Null));
        assert!(matches!(&rows[0][0], Value::Text(t) if t == "cpu"));
    }

    /// An error can arrive at the top OR inside a result, and the second is
    /// easy to miss — it shows as an empty grid instead of a message.
    #[test]
    fn a_v1_error_is_found_wherever_it_is() {
        let e = parse_v1(&json!({ "error": "database not found: nope" })).unwrap_err().to_string();
        assert!(e.contains("database not found"), "{e}");
        let e = parse_v1(&json!({ "results": [{ "error": "error parsing query" }] }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("error parsing query"), "{e}");
    }

    /// Annotated CSV, with its datatypes honoured and its leading empty column
    /// removed from the header AND the rows together.
    #[test]
    fn a_v2_answer_is_read_with_its_declared_types() {
        let csv = "#datatype,string,long,dateTime:RFC3339,double,string\n\
                   #group,false,false,false,false,true\n\
                   ,result,table,_time,_value,_measurement\n\
                   ,_result,0,2026-01-01T12:00:00Z,0.5,cpu\n\
                   ,_result,0,2026-01-01T12:01:00Z,0.6,cpu\n";
        let (cols, rows) = parse_v2_csv(csv);
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["result", "table", "_time", "_value", "_measurement"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 5, "the leading empty column goes from the rows too");
        assert!(matches!(rows[0][1], Value::Int(0)));
        assert!(matches!(&rows[0][2], Value::Timestamp(t) if t == "2026-01-01T12:00:00Z"));
        assert!(matches!(rows[0][3], Value::Float(f) if (f - 0.5).abs() < 1e-9));
        assert!(matches!(&rows[0][4], Value::Text(t) if t == "cpu"));
    }

    #[test]
    fn an_empty_v2_answer_is_no_rows() {
        let (_, rows) = parse_v2_csv("#datatype,string\n\n");
        assert!(rows.is_empty());
    }

    /// Flux's `to()` is the only way a read becomes a write there, and missing
    /// it would let a script write through a read-only connection.
    #[test]
    fn a_flux_script_that_writes_is_a_write() {
        assert!(writes_in(r#"from(bucket:"a") |> range(start:-1h) |> to(bucket:"b")"#));
        assert!(writes_in(r#"from(bucket:"a") |>to(bucket:"b")"#));
        // THE one that found a real bug: every Flux script begins `from(`, and
        // the SQL rule treats an unrecognised leading verb as a write — so a
        // read-only connection refused every Flux READ.
        assert!(!writes_in(r#"from(bucket:"a") |> range(start:-1h)"#));
        assert!(!writes_in("import \"date\"\nfrom(bucket:\"a\")"));
        assert!(classify_in(r#"from(bucket:"a") |> range(start:-1h)"#).is_none());
        // And InfluxQL is still judged as InfluxQL.
        assert!(writes_in("INSERT INTO cpu VALUES (1)"));
        assert!(!writes_in("SELECT * FROM cpu"));
        let why = classify_in(r#"from(bucket:"a") |> to(bucket:"b")"#).unwrap();
        assert!(why.contains("WRITES"), "{why}");
    }

    /// InfluxQL's own destructive verbs, which the SQL rule does not know.
    #[test]
    fn influxqls_own_destructive_verbs_are_named() {
        assert!(classify_in("DROP MEASUREMENT cpu").unwrap().contains("every point"));
        assert!(classify_in("DROP DATABASE metrics").unwrap().contains("everything in it"));
        assert!(classify_in("DROP SERIES FROM cpu").unwrap().contains("matching series"));
        assert!(classify_in("DELETE FROM cpu").unwrap().contains("removes points"));
        // And an ordinary read is still not dangerous.
        assert!(classify_in("SELECT * FROM cpu LIMIT 10").is_none());
        assert!(!writes_in("SELECT * FROM cpu"));
    }

    /// One flag cannot be both SQL and Flux, so it is false — offering
    /// SQL-shaped helpers for Flux would be offering the wrong thing to half
    /// the connections.
    #[test]
    fn it_does_not_claim_to_be_sql() {
        assert!(!META.caps.sql);
        assert!(META.time_filter.contains("range(start:"));
        assert!(!META.time_filter.contains("{col}"));
    }
}
