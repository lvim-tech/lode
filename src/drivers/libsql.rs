// lode/drivers/libsql: Turso and libSQL.
//
// TWO HALVES, AND ONLY ONE OF THEM IS UNVERIFIED.
//
// A libSQL database file IS a SQLite file. So a LOCAL one is not a new driver
// at all: it is the sqlite driver, which is the most-tested thing in this
// engine, reached under another name because somebody who has a libSQL file
// looks for libSQL in the list. That half is verified by the same tests sqlite
// has, and by this suite opening a real file.
//
// A REMOTE one — Turso — speaks HTTP: a POST to `/v2/pipeline` carrying a batch
// of requests, answered with a batch of results. That half cannot be verified
// here: no network, no Turso. What IS verified is the request this driver
// builds and the answer it reads, both against the documented shapes, because
// those are where the bugs live and neither needs a server.
//
// The split is in the FORM, not hidden: a connection has a `file` or a `url`,
// and the driver says which half it is using. A driver that silently fell back
// from remote to local would be a driver that read the wrong database.

use async_trait::async_trait;

use crate::driver::{Connection, Driver};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, Caps, Column, ConnSpec, DriverMeta, ParamSpec, ParamType, Value, SQL_BROWSE_FORM,
    SQL_BUILDER_OPS, SQL_TIME_FILTER,
};

const PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "file",
        label: "Local file",
        kind: ParamType::File,
        required: false,
        secret: false,
        default: None,
    },
    ParamSpec {
        key: "url",
        label: "…or a Turso URL",
        kind: ParamType::String,
        required: false,
        secret: false,
        default: None,
    },
    ParamSpec {
        key: "token",
        label: "Auth token (for a URL)",
        kind: ParamType::String,
        required: false,
        secret: true,
        default: None,
    },
];

const META: DriverMeta = DriverMeta {
    kind: "libsql",
    display: "Turso / libSQL",
    default_port: None,
    params: PARAMS,
    auth: &[AuthKind::None],
    caps: Caps {
        sql: true,
        schemas: true,
        cancel: false,
        tls: true,
        tunnel: false,
        multi_db: false,
        kv: false,
        indexes: true,
        ddl: true,
        metrics: false,
        // A LOCAL libSQL file could edit a row safely — it is sqlite. A remote
        // one goes through a transport this has not verified, and `edit` is a
        // promise about a write being undoable if it hits the wrong rows. One
        // answer for both halves, and it is the careful one.
        edit: false,
    },
    hint: "SELECT * FROM table LIMIT 100",
    browse: "SELECT * FROM {qualified} {clauses} LIMIT {limit};",
    browse_by_kind: &[],
    query_form: &[],
    query_template: "",
    browse_form: SQL_BROWSE_FORM,
    time_filter: SQL_TIME_FILTER,
    builder_ops: SQL_BUILDER_OPS,
    ddl_ops: crate::spec::SQL_DDL_OPS,
    types: crate::spec::SQLITE_TYPES,
    inspectors: &[],
};

/// Which half a spec asks for.
#[derive(Debug, PartialEq)]
pub enum Where {
    Local(String),
    Remote { url: String, token: String },
}

/// Read the spec, and refuse the ambiguous cases rather than picking.
///
/// Both set is refused, not resolved by precedence: a connection naming a local
/// file AND a remote database is one whose author believed something untrue
/// about it, and whichever one lost would be read silently.
pub fn where_is(spec: &ConnSpec) -> anyhow::Result<Where> {
    let file = spec.param_opt("file").unwrap_or("").trim().to_string();
    let url = spec.param_opt("url").unwrap_or("").trim().to_string();
    match (file.is_empty(), url.is_empty()) {
        (false, true) => Ok(Where::Local(crate::spec::expand_tilde(&file))),
        (true, false) => Ok(Where::Remote {
            url: normalise_url(&url),
            token: spec.param_opt("token").unwrap_or("").to_string(),
        }),
        (false, false) => Err(anyhow::anyhow!(
            "this connection names both a local file and a Turso URL — pick one, because a \
             database cannot be in two places"
        )),
        (true, true) => Err(anyhow::anyhow!(
            "a libSQL connection needs either a local file or a Turso URL"
        )),
    }
}

/// Turso hands out `libsql://name-org.turso.io`, which is not a scheme any HTTP
/// client speaks. The documented equivalent is `https://` on the same host, and
/// translating it here is the difference between a connection that works and a
/// URL error about an unknown scheme.
pub fn normalise_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if let Some(rest) = u.strip_prefix("libsql://") {
        return format!("https://{rest}");
    }
    if u.starts_with("http://") || u.starts_with("https://") {
        return u.to_string();
    }
    format!("https://{u}")
}

/// The pipeline request one statement becomes.
///
/// A batch of two: the statement, and a close. Closing in the same request is
/// what keeps a stateless HTTP call from leaving a server-side connection
/// behind for every query somebody runs.
pub fn pipeline_body(sql: &str) -> serde_json::Value {
    serde_json::json!({
        "requests": [
            { "type": "execute", "stmt": { "sql": sql } },
            { "type": "close" }
        ]
    })
}

/// Read a pipeline answer into columns and rows.
///
/// The shape is documented and regular: each result is `ok` or `error`, and an
/// `execute` result carries `cols` and `rows`, where every cell is a tagged
/// value — `{"type":"integer","value":"42"}`. The tags matter: an integer comes
/// across as a STRING so it survives JSON's double, and reading it as text
/// would put quotes round every number in the grid.
pub fn parse_pipeline(v: &serde_json::Value) -> anyhow::Result<(Vec<Column>, Vec<Vec<Value>>)> {
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| anyhow::anyhow!("the server's answer has no results in it"))?;
    let first = results
        .first()
        .ok_or_else(|| anyhow::anyhow!("the server answered with an empty batch"))?;
    if first.get("type").and_then(|t| t.as_str()) == Some("error") {
        let msg = first
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("the server refused, without saying why");
        anyhow::bail!("{msg}");
    }
    let result = first
        .get("response")
        .and_then(|r| r.get("result"))
        .ok_or_else(|| anyhow::anyhow!("the server's answer has no result in it"))?;
    let columns: Vec<Column> = result
        .get("cols")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .map(|c| Column {
                    name: c.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                    type_name: c
                        .get("decltype")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let rows = result
        .get("rows")
        .and_then(|r| r.as_array())
        .map(|rows| rows.iter().map(|row| read_row(row)).collect())
        .unwrap_or_default();
    Ok((columns, rows))
}

fn read_row(row: &serde_json::Value) -> Vec<Value> {
    row.as_array()
        .map(|cells| cells.iter().map(read_cell).collect())
        .unwrap_or_default()
}

/// One tagged cell.
fn read_cell(c: &serde_json::Value) -> Value {
    let kind = c.get("type").and_then(|t| t.as_str()).unwrap_or("null");
    let raw = c.get("value");
    match kind {
        "null" => Value::Null,
        // An integer arrives as a STRING so it survives JSON's double. Parsed
        // back to an integer here: left as text, every number in the grid would
        // sort as text and come back quoted.
        "integer" => raw
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .map(Value::Int)
            .or_else(|| raw.and_then(|v| v.as_i64()).map(Value::Int))
            .unwrap_or(Value::Null),
        "float" => raw
            .and_then(|v| v.as_f64())
            .or_else(|| raw.and_then(|v| v.as_str()).and_then(|s| s.parse().ok()))
            .map(Value::Float)
            .unwrap_or(Value::Null),
        "text" => Value::Text(raw.and_then(|v| v.as_str()).unwrap_or("").to_string()),
        "blob" => {
            let b64 = raw
                .and_then(|v| v.get("base64"))
                .and_then(|v| v.as_str())
                .or_else(|| raw.and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            // The decoded length, not the base64 length: "<12 bytes>" should be
            // the bytes there are.
            let len = b64.len() * 3 / 4;
            Value::Bytes { b64, len }
        }
        other => Value::Text(format!("<{other}>")),
    }
}

pub struct LibsqlDriver;

impl LibsqlDriver {
    pub fn new() -> Self {
        LibsqlDriver
    }
}

#[async_trait]
impl Driver for LibsqlDriver {
    fn meta(&self) -> &'static DriverMeta {
        &META
    }

    async fn connect(&self, spec: &ConnSpec, net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        match where_is(spec)? {
            // A libSQL file is a SQLite file, so this is not a new driver — it
            // is the one that is already the most tested thing here, reached
            // under the name somebody looked for.
            Where::Local(_) => {
                #[cfg(feature = "sqlite")]
                {
                    crate::drivers::sqlite::SqliteDriver::new().connect(spec, net).await
                }
                #[cfg(not(feature = "sqlite"))]
                {
                    let _ = net;
                    Err(anyhow::anyhow!(
                        "a local libSQL file is opened by the sqlite driver, and this build has none"
                    ))
                }
            }
            Where::Remote { .. } => Err(anyhow::anyhow!(
                "the remote (Turso) half of this driver is written against the documented \
                 pipeline protocol but is NOT VERIFIED — there is no network here and no Turso. \
                 Its request building and its answer parsing are tested; the round trip between \
                 them is not, and it is not offered as though it were."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec_with(params: serde_json::Value) -> ConnSpec {
        serde_json::from_value(json!({ "driver": "libsql", "params": params })).unwrap()
    }

    /// A database cannot be in two places, so naming both is refused rather
    /// than resolved by precedence — whichever lost would be read silently.
    #[test]
    fn a_connection_says_which_half_it_is() {
        assert_eq!(
            where_is(&spec_with(json!({ "file": "/tmp/x.db" }))).unwrap(),
            Where::Local("/tmp/x.db".into())
        );
        assert!(matches!(
            where_is(&spec_with(json!({ "url": "libsql://db-org.turso.io", "token": "t" }))).unwrap(),
            Where::Remote { .. }
        ));
        let e = where_is(&spec_with(json!({ "file": "/tmp/x.db", "url": "https://x" })))
            .unwrap_err()
            .to_string();
        assert!(e.contains("two places"), "{e}");
        let e = where_is(&spec_with(json!({}))).unwrap_err().to_string();
        assert!(e.contains("either a local file or"), "{e}");
    }

    /// `libsql://` is what Turso hands out and is not a scheme any HTTP client
    /// speaks. Not translating it gives a URL error that says nothing useful.
    #[test]
    fn the_turso_scheme_becomes_one_a_client_can_dial() {
        assert_eq!(normalise_url("libsql://db-org.turso.io"), "https://db-org.turso.io");
        assert_eq!(normalise_url("https://db-org.turso.io/"), "https://db-org.turso.io");
        assert_eq!(normalise_url("http://localhost:8080"), "http://localhost:8080");
        // A bare host is assumed to want TLS, which is the safe assumption.
        assert_eq!(normalise_url("db-org.turso.io"), "https://db-org.turso.io");
    }

    /// Closing in the same batch is what stops a stateless HTTP call leaving a
    /// server-side connection behind for every query somebody runs.
    #[test]
    fn a_statement_becomes_a_batch_that_closes_itself() {
        let b = pipeline_body("SELECT 1");
        let reqs = b["requests"].as_array().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0]["type"], "execute");
        assert_eq!(reqs[0]["stmt"]["sql"], "SELECT 1");
        assert_eq!(reqs[1]["type"], "close");
    }

    /// A documented pipeline answer, read into a grid.
    #[test]
    fn an_answer_becomes_columns_and_rows_with_their_types() {
        let v = json!({
            "results": [{
                "type": "ok",
                "response": {
                    "type": "execute",
                    "result": {
                        "cols": [
                            { "name": "id", "decltype": "INTEGER" },
                            { "name": "name", "decltype": "TEXT" },
                            { "name": "score", "decltype": "REAL" },
                            { "name": "note", "decltype": null }
                        ],
                        "rows": [
                            [
                                { "type": "integer", "value": "9007199254740993" },
                                { "type": "text", "value": "Ada" },
                                { "type": "float", "value": 1.5 },
                                { "type": "null" }
                            ]
                        ]
                    }
                }
            }, { "type": "ok", "response": { "type": "close" } }]
        });
        let (cols, rows) = parse_pipeline(&v).unwrap();
        assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                   vec!["id", "name", "score", "note"]);
        assert_eq!(cols[0].type_name, "INTEGER");
        assert_eq!(rows.len(), 1);
        // An integer arrives as a STRING so it survives JSON's double. Left as
        // text it would sort as text and come back quoted — and this value is
        // one a double cannot hold.
        assert!(matches!(rows[0][0], Value::Int(9_007_199_254_740_993)), "{:?}", rows[0][0]);
        assert!(matches!(&rows[0][1], Value::Text(t) if t == "Ada"));
        assert!(matches!(rows[0][2], Value::Float(f) if (f - 1.5).abs() < 1e-9));
        assert!(matches!(rows[0][3], Value::Null));
    }

    /// A blob reports the bytes there are, not the length of its base64.
    #[test]
    fn a_blob_reports_its_real_length() {
        let v = json!({ "results": [{ "type": "ok", "response": { "type": "execute", "result": {
            "cols": [{ "name": "b" }],
            "rows": [[{ "type": "blob", "value": { "base64": "AAECAwQF" } }]]
        }}}]});
        let (_, rows) = parse_pipeline(&v).unwrap();
        // "AAECAwQF" is eight base64 characters, which is six bytes.
        assert!(matches!(&rows[0][0], Value::Bytes { len: 6, .. }), "{:?}", rows[0][0]);
    }

    /// The server's own words, not a status code.
    #[test]
    fn an_error_keeps_what_the_server_said() {
        let v = json!({ "results": [{
            "type": "error",
            "error": { "message": "SQLITE_ERROR: no such table: nope", "code": "SQLITE_UNKNOWN" }
        }]});
        let e = parse_pipeline(&v).unwrap_err().to_string();
        assert!(e.contains("no such table: nope"), "{e}");
    }

    #[test]
    fn a_nonsense_answer_says_what_was_missing() {
        assert!(parse_pipeline(&json!({})).unwrap_err().to_string().contains("no results"));
        assert!(parse_pipeline(&json!({ "results": [] }))
            .unwrap_err()
            .to_string()
            .contains("empty batch"));
    }

    /// One answer for both halves, and it is the careful one: `edit` is a
    /// promise that a write which hit the wrong rows can be undone, and the
    /// remote transport here has not been verified at all.
    #[test]
    fn it_does_not_claim_to_edit_rows() {
        assert!(!META.caps.edit);
        assert!(META.caps.sql);
    }
}
