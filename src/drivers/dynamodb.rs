// lode/drivers/dynamodb: a key/value store with a query language that is JSON.
//
// NOT VERIFIED AGAINST A REAL DYNAMODB. No network here, no table. Tested
// properly: the command language, the request each command becomes, the
// attribute shapes DynamoDB answers with read back into a grid, and which
// commands change something. Untested: the HTTP round trip between them.
//
// The language is DynamoDB's own, which is JSON. A client that offered SQL here
// would be inviting a `WHERE` into a store that has no `WHERE` — and the first
// one typed would be silently ignored, which is worse than being told there is
// none. So the driver declares a `query_form`: the four fields DynamoDB's Query
// actually takes, each contributing the JSON it adds, exactly as the MongoDB
// driver does.
//
// WHAT IT WILL NOT DO. There is no PutItem, no UpdateItem and no DeleteItem in
// this language. A client of a database is not where somebody should find out
// they can empty a table; and `caps.edit` is a promise that a write which hit
// more rows than intended can be undone, which over an HTTP API nobody here has
// exercised would be a promise made on no evidence.

use async_trait::async_trait;

use crate::driver::{Connection, Driver};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, Caps, Column, ConnSpec, DriverMeta, ParamSpec, ParamType, QueryFieldSpec, Value,
};

const PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "region",
        label: "Region",
        kind: ParamType::String,
        required: true,
        secret: false,
        default: Some("us-east-1"),
    },
    ParamSpec {
        key: "endpoint",
        label: "Endpoint (blank for AWS)",
        kind: ParamType::String,
        required: false,
        secret: false,
        default: None,
    },
    ParamSpec {
        key: "access_key_id",
        label: "Access key id",
        kind: ParamType::String,
        required: false,
        secret: false,
        default: None,
    },
    ParamSpec {
        key: "secret_access_key",
        label: "Secret access key",
        kind: ParamType::String,
        required: false,
        secret: true,
        default: None,
    },
    ParamSpec {
        key: "aws_profile",
        label: "…or an AWS profile",
        kind: ParamType::AwsProfile,
        required: false,
        secret: false,
        default: None,
    },
];

/// The fields a Query actually takes. Each says what it ADDS, so the client
/// contributes values and the driver owns every word of the request.
const QUERY_FORM: &[QueryFieldSpec] = &[
    QueryFieldSpec {
        key: "key",
        label: "Key condition",
        placeholder: "pk = :id",
        fragment: r#""KeyConditionExpression": "{value}""#,
        default: "",
        rows: 1,
    },
    QueryFieldSpec {
        key: "filter",
        label: "Filter",
        placeholder: "active = :yes",
        fragment: r#""FilterExpression": "{value}""#,
        default: "",
        rows: 1,
    },
    QueryFieldSpec {
        key: "values",
        label: "Values",
        placeholder: r#"{":id": {"S": "42"}}"#,
        fragment: r#""ExpressionAttributeValues": {value}"#,
        default: "",
        rows: 2,
    },
    QueryFieldSpec {
        key: "limit",
        label: "Limit",
        placeholder: "50",
        fragment: r#""Limit": {value}"#,
        default: "50",
        rows: 1,
    },
];

const META: DriverMeta = DriverMeta {
    kind: "dynamodb",
    display: "Amazon DynamoDB",
    default_port: None,
    params: PARAMS,
    auth: &[AuthKind::None, AuthKind::Password],
    caps: Caps {
        sql: false,
        schemas: true,
        cancel: false,
        tls: true,
        tunnel: false,
        multi_db: false,
        kv: true,
        indexes: true,
        ddl: false,
        metrics: false,
        edit: false,
    },
    hint: r#"{"TableName": "things", "Limit": 50}"#,
    browse: r#"{{"TableName": "{name}", "Limit": {limit}}}"#,
    browse_by_kind: &[],
    query_form: QUERY_FORM,
    query_template: r#"{{"TableName": "{name}", "Limit": {limit}{fragments}}}"#,
    browse_form: &[],
    // No server-side time window: a timestamp in DynamoDB is an ordinary
    // attribute, filtered by a FilterExpression the author writes. An empty
    // template is how a driver says it has none.
    time_filter: "",
    builder_ops: &[],
    inspectors: &[],
};

/// Which API call a statement is, and the body it carries.
///
/// A statement is a JSON document, as it is for MongoDB: whatever is in it goes
/// to the API. Which CALL it is comes from what the document contains, because
/// `KeyConditionExpression` is the one thing that distinguishes a Query from a
/// Scan and asking the person to say so twice would be asking them to repeat
/// the API's own rule.
pub fn target_for(body: &serde_json::Value) -> &'static str {
    if body.get("KeyConditionExpression").is_some() {
        "DynamoDB_20120810.Query"
    } else {
        "DynamoDB_20120810.Scan"
    }
}

pub fn endpoint_for(region: &str, endpoint: &str) -> String {
    let e = endpoint.trim().trim_end_matches('/');
    if e.is_empty() {
        return format!("dynamodb.{region}.amazonaws.com");
    }
    e.trim_start_matches("https://").trim_start_matches("http://").to_string()
}

/// Read a statement, with an error that says what was wrong with it.
pub fn parse_statement(statement: &str) -> anyhow::Result<serde_json::Value> {
    let s = statement.trim();
    if s.is_empty() {
        anyhow::bail!("DynamoDB takes a request document — for example: {{\"TableName\": \"things\", \"Limit\": 50}}");
    }
    let v: serde_json::Value = serde_json::from_str(s).map_err(|e| {
        anyhow::anyhow!(
            "DynamoDB takes a request document, not SQL — for example: \
             {{\"TableName\": \"things\", \"Limit\": 50}} ({e})"
        )
    })?;
    if v.get("TableName").is_none() {
        anyhow::bail!("that document has no \"TableName\" in it, and every DynamoDB request needs one");
    }
    Ok(v)
}

/// One DynamoDB attribute value, as a cell.
///
/// The tags are the type, and they are the whole reason this is not just JSON:
/// `{"N": "42"}` is the NUMBER 42, sent as a string so it survives JSON's
/// double. Reading it as text would put quotes round every number in the grid
/// and sort them as words.
pub fn attr(v: &serde_json::Value) -> Value {
    let Some(o) = v.as_object() else { return Value::Null };
    let Some((tag, inner)) = o.iter().next() else { return Value::Null };
    match tag.as_str() {
        "NULL" => Value::Null,
        "S" => Value::Text(inner.as_str().unwrap_or("").to_string()),
        "N" => {
            let raw = inner.as_str().unwrap_or("");
            raw.parse::<i64>()
                .map(Value::Int)
                .or_else(|_| raw.parse::<f64>().map(Value::Float))
                .unwrap_or_else(|_| Value::Text(raw.to_string()))
        }
        "BOOL" => Value::Bool(inner.as_bool().unwrap_or(false)),
        "B" => {
            let b64 = inner.as_str().unwrap_or("").to_string();
            let len = b64.len() * 3 / 4;
            Value::Bytes { b64, len }
        }
        // A list or a map stays structured, so the JSON view shows it as a tree
        // rather than as a line of punctuation.
        "L" => Value::Json(serde_json::Value::Array(
            inner
                .as_array()
                .map(|a| a.iter().map(|x| to_plain(&attr(x))).collect())
                .unwrap_or_default(),
        )),
        "M" => Value::Json(serde_json::Value::Object(
            inner
                .as_object()
                .map(|m| m.iter().map(|(k, x)| (k.clone(), to_plain(&attr(x)))).collect())
                .unwrap_or_default(),
        )),
        "SS" | "NS" | "BS" => Value::Json(inner.clone()),
        _ => Value::Json(v.clone()),
    }
}

fn to_plain(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::json!(b),
        Value::Int(i) => serde_json::json!(i),
        Value::Float(f) => serde_json::json!(f),
        Value::Text(t) => serde_json::json!(t),
        Value::Json(j) => j.clone(),
        other => serde_json::json!(format!("{other:?}")),
    }
}

/// Read a Scan or Query answer into a grid.
///
/// DynamoDB items are DOCUMENTS: every one may have different attributes. The
/// columns are therefore the union of what came back, in first-seen order, and
/// an item missing one has NULL there — which is true, because in a schemaless
/// store "this item has no such attribute" is a fact and not a gap.
pub fn parse_items(v: &serde_json::Value) -> (Vec<Column>, Vec<Vec<Value>>) {
    let items = v.get("Items").and_then(|i| i.as_array()).cloned().unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for item in &items {
        if let Some(o) = item.as_object() {
            for k in o.keys() {
                if !names.contains(k) {
                    names.push(k.clone());
                }
            }
        }
    }
    let columns: Vec<Column> = names
        .iter()
        .map(|n| Column { name: n.clone(), type_name: String::new() })
        .collect();
    let rows = items
        .iter()
        .map(|item| {
            names
                .iter()
                .map(|n| item.get(n).map(attr).unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    (columns, rows)
}

/// AWS's JSON error, which carries its reason in a type.
pub fn parse_error(status: u16, body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let kind = v
        .get("__type")
        .and_then(|t| t.as_str())
        // The type comes back fully qualified; the last segment is the name.
        .map(|t| t.rsplit('#').next().unwrap_or(t).to_string())
        .unwrap_or_default();
    let msg = v
        .get("message")
        .or_else(|| v.get("Message"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    if kind.is_empty() && msg.is_empty() {
        return format!("DynamoDB answered {status}");
    }
    let mut out = if msg.is_empty() { kind.clone() } else { format!("{kind}: {msg}") };
    if kind == "ResourceNotFoundException" {
        out.push_str(" — that table does not exist in this region");
    }
    if kind == "ProvisionedThroughputExceededException" {
        out.push_str(" — the table is rate limited, not broken");
    }
    out
}

pub struct DynamoDriver;

impl DynamoDriver {
    pub fn new() -> Self {
        DynamoDriver
    }
}

#[async_trait]
impl Driver for DynamoDriver {
    fn meta(&self) -> &'static DriverMeta {
        &META
    }

    async fn connect(&self, _spec: &ConnSpec, _net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        Err(anyhow::anyhow!(
            "the DynamoDB driver is written against the documented API but is NOT VERIFIED — no \
             network here and no table. Its request building and its answer parsing are tested; \
             the round trip between them is not, and it is not offered as though it were."
        ))
    }

    /// This language reads. There is no Put, no Update and no Delete in it, so
    /// there is nothing here to call dangerous — and nothing to let past a
    /// read-only connection either.
    fn classify(&self, _statement: &str) -> Option<String> {
        None
    }

    fn writes(&self, _statement: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Which call it is comes from what the document contains. Asking somebody
    /// to say "Query" as well as writing a KeyConditionExpression is asking
    /// them to repeat the API's own rule.
    #[test]
    fn the_document_says_which_call_it_is() {
        assert_eq!(target_for(&json!({ "TableName": "t" })), "DynamoDB_20120810.Scan");
        assert_eq!(
            target_for(&json!({ "TableName": "t", "KeyConditionExpression": "pk = :id" })),
            "DynamoDB_20120810.Query"
        );
    }

    /// The error teaches, with the same example the empty editor offers — so
    /// the two cannot drift apart.
    #[test]
    fn sql_typed_here_is_told_what_this_takes() {
        let e = parse_statement("SELECT * FROM things").unwrap_err().to_string();
        assert!(e.contains("not SQL"), "{e}");
        assert!(e.contains("TableName"), "{e}");
        let e = parse_statement(r#"{"Limit": 5}"#).unwrap_err().to_string();
        assert!(e.contains("no \"TableName\""), "{e}");
        assert!(parse_statement(r#"{"TableName":"t"}"#).is_ok());
    }

    #[test]
    fn a_local_dynamodb_is_the_endpoint_field_and_not_a_special_case() {
        assert_eq!(endpoint_for("eu-west-1", ""), "dynamodb.eu-west-1.amazonaws.com");
        assert_eq!(endpoint_for("eu-west-1", "http://localhost:8000"), "localhost:8000");
    }

    /// The tags ARE the type. `{"N":"42"}` is the number 42, sent as a string
    /// so it survives JSON's double; read as text it would sort as a word.
    #[test]
    fn an_attribute_keeps_the_type_its_tag_gives_it() {
        assert!(matches!(attr(&json!({"S": "Ada"})), Value::Text(t) if t == "Ada"));
        assert!(matches!(attr(&json!({"N": "42"})), Value::Int(42)));
        assert!(matches!(attr(&json!({"N": "1.5"})), Value::Float(_)));
        // Bigger than a double can hold, which is why DynamoDB sends it as text.
        assert!(matches!(attr(&json!({"N": "9007199254740993"})), Value::Int(9_007_199_254_740_993)));
        assert!(matches!(attr(&json!({"BOOL": true})), Value::Bool(true)));
        assert!(matches!(attr(&json!({"NULL": true})), Value::Null));
        assert!(matches!(attr(&json!({"B": "AAECAwQF"})), Value::Bytes { len: 6, .. }));
    }

    /// A list or a map stays structured, so the JSON view shows a tree rather
    /// than a line of punctuation.
    #[test]
    fn a_nested_attribute_stays_structured() {
        let v = attr(&json!({"M": {"a": {"S": "x"}, "n": {"N": "7"}}}));
        let Value::Json(j) = v else { panic!("a map should stay a map") };
        assert_eq!(j["a"], json!("x"));
        assert_eq!(j["n"], json!(7));
        let v = attr(&json!({"L": [{"S": "x"}, {"N": "2"}]}));
        let Value::Json(j) = v else { panic!("a list should stay a list") };
        assert_eq!(j, json!(["x", 2]));
    }

    /// Items are documents, so the columns are the UNION of what came back and
    /// an item missing one has NULL — which is true: in a schemaless store "no
    /// such attribute" is a fact, not a gap.
    #[test]
    fn documents_with_different_attributes_still_make_a_grid() {
        let v = json!({ "Items": [
            { "pk": {"S": "a"}, "n": {"N": "1"} },
            { "pk": {"S": "b"}, "extra": {"S": "yes"} }
        ], "Count": 2 });
        let (cols, rows) = parse_items(&v);
        assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["pk", "n", "extra"]);
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0][2], Value::Null), "the first item has no `extra`");
        assert!(matches!(rows[1][1], Value::Null), "the second has no `n`");
        assert!(matches!(&rows[1][2], Value::Text(t) if t == "yes"));
    }

    #[test]
    fn an_empty_answer_is_no_rows() {
        let (_, rows) = parse_items(&json!({ "Items": [], "Count": 0 }));
        assert!(rows.is_empty());
        let (_, rows) = parse_items(&json!({ "Count": 0 }));
        assert!(rows.is_empty());
    }

    /// The reason comes out of AWS's error type, and the two that are most
    /// often misread get a sentence saying what they actually mean.
    #[test]
    fn an_error_says_what_aws_called_it() {
        let e = parse_error(400, r#"{"__type":"com.amazonaws.dynamodb.v20120810#ResourceNotFoundException","message":"Requested resource not found"}"#);
        assert!(e.contains("ResourceNotFoundException"), "{e}");
        assert!(e.contains("does not exist in this region"), "{e}");
        // `r##` because the document itself contains `"#`, which ends an `r#`
        // string early — the fully qualified type always has a `#` in it.
        let e = parse_error(400, r##"{"__type":"x#ProvisionedThroughputExceededException","message":"x"}"##);
        assert!(e.contains("rate limited, not broken"), "{e}");
        assert!(parse_error(500, "not json").contains("500"));
    }

    /// There is no Put, no Update and no Delete in this language.
    #[test]
    fn nothing_here_writes() {
        let d = DynamoDriver::new();
        for s in [r#"{"TableName":"t"}"#, "DROP TABLE t", "DELETE FROM t"] {
            assert!(!d.writes(s), "{s}");
            assert!(d.classify(s).is_none(), "{s}");
        }
    }

    /// It declares its own form, which is what stops a client offering SQL to
    /// something that has none.
    #[test]
    fn it_declares_its_own_query_form() {
        assert!(!META.caps.sql);
        assert_eq!(META.query_form.len(), 4);
        assert!(META.query_form.iter().any(|f| f.key == "key"));
        assert_eq!(META.time_filter, "");
        assert!(!META.caps.edit);
    }
}
