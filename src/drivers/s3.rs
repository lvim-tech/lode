// lode/drivers/s3: object storage as a browsable thing.
//
// NOT VERIFIED AGAINST A REAL S3. There is no network here and no bucket. What
// IS verified, and tested properly below, is every part that does not need one:
//
//   * the command language — what `ls bucket/prefix` becomes as a request
//   * the URL and the query parameters built from it
//   * the XML the three operations answer with, parsed from real response
//     shapes into the columns and rows a grid shows
//   * which commands change something, which is what `classify` and `writes`
//     are read from
//   * the endpoint arithmetic for AWS, for R2 and for MinIO, which differ in
//     ways that are easy to get subtly wrong
//
// The request goes out over HTTP and the answer comes back; that middle step is
// the unverified one, and it is the only one.
//
// WHY A COMMAND LANGUAGE AND NOT SQL. S3 has no query engine. Pretending
// otherwise — accepting `SELECT * FROM bucket` and translating — would be the
// client's statement box lying about what the far end can do, and the first
// `WHERE` anybody typed would be silently ignored. The driver declares its own
// `hint` and its own `browse` template, which is exactly the mechanism that
// exists for engines whose language is not SQL.

use async_trait::async_trait;

use crate::driver::{Connection, Driver, ResultStream};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, Caps, Column, ConnSpec, DriverMeta, Index, Node, ObjRef, ParamSpec, ParamType,
    RowOutcome, RowPlan, TableColumn, Value,
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
    // Blank for AWS, set for everything that is not. R2 and MinIO are the two
    // that matter and they are not special cases in the code — they are this
    // field with a value in it.
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
    // Named rather than copied: the key stays in the AWS files where the AWS
    // tools rotate it. See access.rs.
    ParamSpec {
        key: "aws_profile",
        label: "…or an AWS profile",
        kind: ParamType::AwsProfile,
        required: false,
        secret: false,
        default: None,
    },
];

const META: DriverMeta = DriverMeta {
    kind: "s3",
    display: "S3 (AWS, R2, MinIO)",
    default_port: None,
    params: PARAMS,
    auth: &[AuthKind::None, AuthKind::Password],
    caps: Caps {
        // Not SQL, and saying so is what stops a client offering a WHERE box
        // for something that has no WHERE.
        sql: false,
        schemas: true,
        cancel: false,
        tls: true,
        tunnel: false,
        multi_db: false,
        kv: true,
        indexes: false,
        ddl: false,
        metrics: false,
        edit: false,
    },
    hint: "ls",
    browse: "ls {name}",
    browse_by_kind: &[("bucket", "ls {name}")],
    query_form: &[],
    query_template: "",
    browse_form: &[],
    // S3 has no server-side time window: listing is by prefix, and filtering by
    // LastModified happens after the answer arrives. An empty template is how a
    // driver says it has none, and the dashboard says so on the tile.
    time_filter: "",
    builder_ops: &[],
};

/// One parsed command.
///
/// The whole language. Four verbs, because S3 has four things anybody wants
/// from a database client and none of them is a join.
#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// Every bucket.
    ListBuckets,
    /// The objects under a prefix.
    List { bucket: String, prefix: String },
    /// One object's metadata.
    Head { bucket: String, key: String },
    /// One object's first bytes.
    Get { bucket: String, key: String },
}

/// Read a command, or say what was expected.
///
/// The error names the verbs rather than saying "syntax error": a language with
/// four words should be able to list them when one is wrong.
pub fn parse(statement: &str) -> anyhow::Result<Cmd> {
    let s = statement.trim();
    let (verb, rest) = match s.split_once(char::is_whitespace) {
        Some((v, r)) => (v.to_ascii_lowercase(), r.trim()),
        None => (s.to_ascii_lowercase(), ""),
    };
    let split = |r: &str| -> anyhow::Result<(String, String)> {
        let r = r.trim().trim_start_matches("s3://");
        match r.split_once('/') {
            Some((b, k)) if !b.is_empty() && !k.is_empty() => Ok((b.to_string(), k.to_string())),
            _ => Err(anyhow::anyhow!(
                "that needs a bucket and a key, as `bucket/key` — '{r}' has only one part"
            )),
        }
    };
    match verb.as_str() {
        "ls" | "list" => {
            let r = rest.trim().trim_start_matches("s3://");
            if r.is_empty() {
                return Ok(Cmd::ListBuckets);
            }
            let (bucket, prefix) = match r.split_once('/') {
                Some((b, p)) => (b.to_string(), p.to_string()),
                None => (r.to_string(), String::new()),
            };
            Ok(Cmd::List { bucket, prefix })
        }
        "head" | "stat" => {
            let (bucket, key) = split(rest)?;
            Ok(Cmd::Head { bucket, key })
        }
        "get" | "cat" => {
            let (bucket, key) = split(rest)?;
            Ok(Cmd::Get { bucket, key })
        }
        "" => Err(anyhow::anyhow!("say what to do: ls, head or get")),
        other => Err(anyhow::anyhow!(
            "'{other}' is not something this understands — it knows ls, head and get"
        )),
    }
}

/// Where a command's request goes: the host, the path and the query.
///
/// `endpoint` empty means AWS, and then the bucket goes in the HOST, which is
/// what AWS wants and what signing depends on. A custom endpoint — R2, MinIO —
/// gets the bucket in the PATH instead, because MinIO is usually one host with
/// many buckets under it and virtual-host style needs DNS nobody has set up.
pub fn endpoint_for(region: &str, endpoint: &str, bucket: Option<&str>) -> (String, String) {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        return match bucket {
            // AWS, virtual-host style.
            Some(b) => (format!("{b}.s3.{region}.amazonaws.com"), String::new()),
            None => (format!("s3.{region}.amazonaws.com"), String::new()),
        };
    }
    let host = endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();
    match bucket {
        // Path style, which is what MinIO and R2 both accept.
        Some(b) => (host, format!("/{b}")),
        None => (host, String::new()),
    }
}

/// The request a command becomes: method, path, query.
pub fn request_for(cmd: &Cmd, region: &str, endpoint: &str) -> (String, String, String, Vec<(String, String)>) {
    match cmd {
        Cmd::ListBuckets => {
            let (host, base) = endpoint_for(region, endpoint, None);
            (host, "GET".into(), format!("{base}/"), vec![])
        }
        Cmd::List { bucket, prefix } => {
            let (host, base) = endpoint_for(region, endpoint, Some(bucket));
            let mut q = vec![("list-type".to_string(), "2".to_string())];
            if !prefix.is_empty() {
                q.push(("prefix".into(), prefix.clone()));
            }
            // A slash-delimited listing, so a prefix behaves like a folder
            // rather than returning every object under it recursively — which
            // on a large bucket is the difference between a page and a bill.
            q.push(("delimiter".into(), "/".into()));
            q.push(("max-keys".into(), "1000".into()));
            (host, "GET".into(), format!("{base}/"), q)
        }
        Cmd::Head { bucket, key } => {
            let (host, base) = endpoint_for(region, endpoint, Some(bucket));
            (host, "HEAD".into(), format!("{base}/{key}"), vec![])
        }
        Cmd::Get { bucket, key } => {
            let (host, base) = endpoint_for(region, endpoint, Some(bucket));
            (host, "GET".into(), format!("{base}/{key}"), vec![])
        }
    }
}

// ── the XML, read without an XML parser ──────────────────────────────────────
//
// S3's list responses are flat and regular: repeated `<Contents>` or `<Bucket>`
// blocks with a handful of leaf elements each. A scanner for exactly that is
// twenty lines and is tested against real response shapes; a general parser
// would be a dependency and the same twenty lines of extraction on top.

/// Every `<tag>…</tag>` body, in order.
pub fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        match after.find(&close) {
            Some(j) => {
                out.push(&after[..j]);
                rest = &after[j + close.len()..];
            }
            None => break,
        }
    }
    out
}

/// The first `<tag>` body, or empty.
pub fn field(xml: &str, tag: &str) -> String {
    blocks(xml, tag)
        .first()
        .map(|s| unescape(s))
        .unwrap_or_default()
}

/// The five XML entities. S3 escapes keys, and a key with an ampersand in it
/// comes back as `&amp;` — shown raw it would be a key nobody can copy.
fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// A listing of buckets, as a grid.
pub fn parse_buckets(xml: &str) -> (Vec<Column>, Vec<Vec<Value>>) {
    let cols = vec![col("name"), col("created")];
    let rows = blocks(xml, "Bucket")
        .into_iter()
        .map(|b| {
            vec![
                Value::Text(field(b, "Name")),
                Value::Text(field(b, "CreationDate")),
            ]
        })
        .collect();
    (cols, rows)
}

/// A listing of objects. Prefixes come FIRST and are marked, because a listing
/// where folders and objects are mixed is a listing somebody has to re-sort.
pub fn parse_objects(xml: &str) -> (Vec<Column>, Vec<Vec<Value>>) {
    let cols = vec![col("key"), col("kind"), col("size"), col("modified"), col("etag")];
    let mut rows: Vec<Vec<Value>> = blocks(xml, "CommonPrefixes")
        .into_iter()
        .map(|p| {
            vec![
                Value::Text(field(p, "Prefix")),
                Value::Text("prefix".into()),
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        })
        .collect();
    for c in blocks(xml, "Contents") {
        let size = field(c, "Size").parse::<i64>().ok();
        rows.push(vec![
            Value::Text(field(c, "Key")),
            Value::Text("object".into()),
            size.map(Value::Int).unwrap_or(Value::Null),
            Value::Text(field(c, "LastModified")),
            Value::Text(field(c, "ETag").trim_matches('"').to_string()),
        ]);
    }
    (cols, rows)
}

fn col(name: &str) -> Column {
    Column {
        name: name.to_string(),
        type_name: String::new(),
    }
}

/// S3's own error document, which carries the reason in it.
///
/// Read rather than reported as a status code: "NoSuchBucket" and
/// "SignatureDoesNotMatch" are different problems and "403" is both.
pub fn parse_error(status: u16, xml: &str) -> String {
    let code = field(xml, "Code");
    let message = field(xml, "Message");
    if code.is_empty() {
        return format!("S3 answered {status}");
    }
    let mut out = format!("{code}: {message}");
    if code == "SignatureDoesNotMatch" {
        out.push_str(" — the key, the secret, or the region is wrong");
    }
    if code == "AccessDenied" {
        out.push_str(" — the credentials are valid and are not allowed to do this");
    }
    out
}

pub struct S3Driver;

impl S3Driver {
    pub fn new() -> Self {
        S3Driver
    }
}

#[async_trait]
impl Driver for S3Driver {
    fn meta(&self) -> &'static DriverMeta {
        &META
    }

    async fn connect(&self, _spec: &ConnSpec, _net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        Err(anyhow::anyhow!(
            "the S3 driver's network half is written but NOT verified — no bucket and no network \
             here. It is registered so its command language and its metadata can be seen; opening \
             a connection is deliberately refused rather than offered untested."
        ))
    }

    /// Nothing in this language changes anything — there is no delete verb, on
    /// purpose. A client of a database is not where somebody should discover
    /// that they can empty a bucket with one word.
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

    #[test]
    fn the_four_verbs_parse_and_the_rest_are_named() {
        assert_eq!(parse("ls").unwrap(), Cmd::ListBuckets);
        assert_eq!(parse("  LS  ").unwrap(), Cmd::ListBuckets);
        assert_eq!(
            parse("ls photos").unwrap(),
            Cmd::List { bucket: "photos".into(), prefix: String::new() }
        );
        assert_eq!(
            parse("ls photos/2026/").unwrap(),
            Cmd::List { bucket: "photos".into(), prefix: "2026/".into() }
        );
        // The s3:// somebody pastes from the console.
        assert_eq!(
            parse("ls s3://photos/2026/").unwrap(),
            Cmd::List { bucket: "photos".into(), prefix: "2026/".into() }
        );
        assert_eq!(
            parse("head photos/a.jpg").unwrap(),
            Cmd::Head { bucket: "photos".into(), key: "a.jpg".into() }
        );
        assert_eq!(
            parse("get photos/deep/a.jpg").unwrap(),
            Cmd::Get { bucket: "photos".into(), key: "deep/a.jpg".into() }
        );
    }

    /// A language with four words should list them when one is wrong, rather
    /// than saying "syntax error".
    #[test]
    fn an_unknown_verb_names_the_ones_there_are() {
        let e = parse("SELECT * FROM photos").unwrap_err().to_string();
        assert!(e.contains("ls, head and get"), "{e}");
        let e = parse("head photos").unwrap_err().to_string();
        assert!(e.contains("bucket and a key"), "{e}");
    }

    /// AWS puts the bucket in the host; a custom endpoint puts it in the path.
    /// Getting this backwards gives a signature over the wrong canonical
    /// request and an error that says nothing about hosts.
    #[test]
    fn aws_r2_and_minio_address_a_bucket_differently() {
        assert_eq!(
            endpoint_for("eu-west-1", "", Some("photos")),
            ("photos.s3.eu-west-1.amazonaws.com".into(), String::new())
        );
        assert_eq!(
            endpoint_for("eu-west-1", "", None),
            ("s3.eu-west-1.amazonaws.com".into(), String::new())
        );
        // R2.
        assert_eq!(
            endpoint_for("auto", "https://abc123.r2.cloudflarestorage.com", Some("photos")),
            ("abc123.r2.cloudflarestorage.com".into(), "/photos".into())
        );
        // MinIO, with a port and no scheme.
        assert_eq!(
            endpoint_for("us-east-1", "minio.internal:9000/", Some("photos")),
            ("minio.internal:9000".into(), "/photos".into())
        );
    }

    /// A prefix listing is slash-delimited, so a prefix behaves like a folder.
    /// Without the delimiter a listing of a large bucket returns everything
    /// under it, which is the difference between a page and a bill.
    #[test]
    fn a_listing_is_delimited_and_capped() {
        let (_host, method, path, q) = request_for(
            &Cmd::List { bucket: "photos".into(), prefix: "2026/".into() },
            "eu-west-1",
            "",
        );
        assert_eq!(method, "GET");
        assert_eq!(path, "/");
        assert!(q.contains(&("list-type".into(), "2".into())));
        assert!(q.contains(&("prefix".into(), "2026/".into())));
        assert!(q.contains(&("delimiter".into(), "/".into())));
        assert!(q.iter().any(|(k, _)| k == "max-keys"));
    }

    #[test]
    fn head_and_get_address_the_object_itself() {
        let (host, method, path, _) =
            request_for(&Cmd::Head { bucket: "photos".into(), key: "a/b.jpg".into() }, "eu-west-1", "");
        assert_eq!(host, "photos.s3.eu-west-1.amazonaws.com");
        assert_eq!(method, "HEAD");
        assert_eq!(path, "/a/b.jpg");
    }

    /// A real ListAllMyBuckets answer.
    #[test]
    fn a_bucket_listing_becomes_rows() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListAllMyBucketsResult><Owner><ID>x</ID></Owner><Buckets>
<Bucket><Name>photos</Name><CreationDate>2026-01-05T10:00:00.000Z</CreationDate></Bucket>
<Bucket><Name>backups</Name><CreationDate>2025-11-02T09:00:00.000Z</CreationDate></Bucket>
</Buckets></ListAllMyBucketsResult>"#;
        let (cols, rows) = parse_buckets(xml);
        assert_eq!(cols.len(), 2);
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0][0], Value::Text(t) if t == "photos"));
        assert!(matches!(&rows[1][0], Value::Text(t) if t == "backups"));
    }

    /// A real ListObjectsV2 answer, with a folder and two objects. Prefixes
    /// come first and are marked: a listing where folders and objects are mixed
    /// is one somebody has to re-sort.
    #[test]
    fn an_object_listing_puts_folders_first_and_says_which_is_which() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult><Name>photos</Name><Prefix>2026/</Prefix><KeyCount>3</KeyCount>
<CommonPrefixes><Prefix>2026/january/</Prefix></CommonPrefixes>
<Contents><Key>2026/a.jpg</Key><LastModified>2026-01-05T10:00:00.000Z</LastModified>
<ETag>&quot;9bb58f26192e4ba00f01e2e7b136bbd8&quot;</ETag><Size>4096</Size></Contents>
<Contents><Key>2026/b &amp; c.jpg</Key><LastModified>2026-01-06T10:00:00.000Z</LastModified>
<ETag>&quot;abc&quot;</ETag><Size>12</Size></Contents>
</ListBucketResult>"#;
        let (cols, rows) = parse_objects(xml);
        assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                   vec!["key", "kind", "size", "modified", "etag"]);
        assert_eq!(rows.len(), 3);
        assert!(matches!(&rows[0][1], Value::Text(t) if t == "prefix"));
        assert!(matches!(&rows[0][0], Value::Text(t) if t == "2026/january/"));
        // A size is a NUMBER, so the grid sorts it as one.
        assert!(matches!(&rows[1][2], Value::Int(4096)));
        // The quotes AWS wraps an ETag in are not part of it.
        assert!(matches!(&rows[1][4], Value::Text(t) if t == "9bb58f26192e4ba00f01e2e7b136bbd8"));
        // A key with an ampersand comes back escaped; shown raw it would be a
        // key nobody can copy.
        assert!(matches!(&rows[2][0], Value::Text(t) if t == "2026/b & c.jpg"), "{:?}", rows[2][0]);
    }

    /// An empty listing is empty rather than wrong.
    #[test]
    fn an_empty_listing_is_no_rows_and_not_an_error() {
        let (_, rows) = parse_objects("<ListBucketResult><Name>x</Name><KeyCount>0</KeyCount></ListBucketResult>");
        assert!(rows.is_empty());
    }

    /// The reason comes out of S3's error document. "NoSuchBucket" and
    /// "SignatureDoesNotMatch" are different problems and "403" is both.
    #[test]
    fn an_error_keeps_the_reason_s3_gave() {
        let xml = r#"<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message></Error>"#;
        let e = parse_error(404, xml);
        assert!(e.contains("NoSuchBucket"), "{e}");
        assert!(e.contains("does not exist"), "{e}");

        let xml = r#"<Error><Code>SignatureDoesNotMatch</Code><Message>...</Message></Error>"#;
        let e = parse_error(403, xml);
        assert!(e.contains("region is wrong"), "{e}");

        // No document at all: say the status rather than nothing.
        assert!(parse_error(500, "").contains("500"));
    }

    /// There is no delete verb, and that is deliberate: a database client is
    /// not where somebody should find out they can empty a bucket with a word.
    #[test]
    fn nothing_in_this_language_changes_anything() {
        let d = S3Driver::new();
        for s in ["ls", "get photos/a.jpg", "DROP TABLE photos", "DELETE FROM photos"] {
            assert!(!d.writes(s), "{s}");
            assert!(d.classify(s).is_none(), "{s}");
        }
        assert!(parse("rm photos/a.jpg").is_err());
    }

    /// Declared NOT SQL, which is what stops a client offering a WHERE box for
    /// something that has no WHERE — and an empty `time_filter`, which is how a
    /// driver says it has no server-side time window.
    #[test]
    fn it_declares_what_it_is_not() {
        assert!(!META.caps.sql);
        assert!(META.caps.kv);
        assert!(!META.caps.edit);
        assert_eq!(META.time_filter, "");
        assert_eq!(META.hint, "ls");
    }
}
