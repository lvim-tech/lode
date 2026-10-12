// lode/drivers/redis: the Redis driver.
//
// Redis is a key/value store, not SQL: the query editor's "statement" is a Redis
// command line (e.g. `GET greeting`, `LRANGE fruits 0 -1`, `HGETALL user:1`).
// The reply is rendered as a grid — a scalar is one row, an array/set is one row
// per element, a map/hash is key/value rows; nested structures become JSON. The
// schema tree is the keyspace: a bounded SCAN lists keys with their type as the
// node kind. Databases are the numbered logical DBs (SELECT switches).

use async_trait::async_trait;
use base64::Engine as _;
use redis::{aio::MultiplexedConnection, Value as RValue};
use serde_json::json;
use tokio::net::TcpStream;

use crate::driver::{Connection, Driver, ResultStream};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, AuthSpec, Caps, Column, ConnSpec, DriverMeta, Node, ObjRef, ParamSpec, ParamType, RowOutcome, RowPlan,
    TableColumn, TlsSpec, Value,
};

const PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "host",
        label: "Host",
        kind: ParamType::String,
        required: true,
        secret: false,
        default: Some("127.0.0.1"),
    },
    ParamSpec {
        key: "port",
        label: "Port",
        kind: ParamType::Int,
        required: false,
        secret: false,
        default: Some("6379"),
    },
    ParamSpec {
        key: "db",
        label: "DB index",
        kind: ParamType::Int,
        required: false,
        secret: false,
        default: Some("0"),
    },
];

/// Reading a Redis key is a different command for every type of key, and the
/// type is exactly what `structure` already labels each node with. So the map is
/// keyed by that label: a client looks the node's kind up and gets the command,
/// without knowing a thing about Redis.
///
/// `browse` itself is `TYPE`, which is the honest answer for a kind not listed
/// here — it says what the key is, and the next command is then obvious.
const BROWSE_BY_KIND: &[(&str, &str)] = &[
    ("string", "GET {name}"),
    ("list", "LRANGE {name} 0 {limit}"),
    ("hash", "HGETALL {name}"),
    ("set", "SMEMBERS {name}"),
    ("zset", "ZRANGE {name} 0 {limit} WITHSCORES"),
    ("stream", "XRANGE {name} - + COUNT {limit}"),
];

const META: DriverMeta = DriverMeta {
    kind: "redis",
    display: "Redis",
    default_port: Some(6379),
    params: PARAMS,
    auth: &[AuthKind::None, AuthKind::Password],
    caps: Caps {
        sql: false,
        schemas: true,
        cancel: false,
        tls: true,
        tunnel: true,
        multi_db: true,
        kv: true,
        indexes: false,
        ddl: false, // a KV store has neither indexes nor a schema to describe
        // A key/value store edits a row like anything else, once the row says
        // WHICH value it is: see `shape_of`, which is the whole of it.
        edit: true,
        // INFO — a read. Unverified here.
        metrics: true,
    },
    hint: "KEYS *",
    browse: "TYPE {name}",
    browse_by_kind: BROWSE_BY_KIND,
    query_form: &[],
    query_template: "",
    browse_form: &[],
    time_filter: "",
    builder_ops: &[],
    ddl_ops: &[],
    types: &[],
    inspectors: REDIS_INSPECTORS,
};

/// What a Redis server will say about itself, live.
///
/// `CLIENT LIST` answers "who is connected and what are they doing", and
/// `CLIENT KILL` is what disconnects one. The reply is field=value pairs per
/// line, so it is parsed into columns here rather than shown as one long
/// string — a page somebody has to read with their finger is not a view.
const REDIS_INSPECTORS: &[crate::spec::Inspector] = &[crate::spec::Inspector {
    name: "clients",
    label: "Who is connected",
    act: "terminate",
}];

/// The Redis driver.
pub struct RedisDriver;

impl RedisDriver {
    pub fn new() -> Self {
        RedisDriver
    }
}

/// What Redis needs to know about the session itself: which database, and who
/// is asking.
///
/// These travel as FIELDS. Percent-encoding a password into a `redis://` URL —
/// and having to get `@`, `#`, `:` and `%` right, or connect somebody to the
/// wrong host with half a password — was the only reason this driver ever built
/// a URL, and nothing needs one now.
async fn connection_info(spec: &ConnSpec) -> anyhow::Result<redis::RedisConnectionInfo> {
    let db: i64 = spec.param_opt("db").unwrap_or("0").parse().unwrap_or(0);
    let (username, password) = match &spec.auth {
        AuthSpec::Password { user, password } => {
            let pw = password.resolve().await?;
            ((!user.is_empty()).then(|| user.clone()), Some(pw))
        }
        _ => (None, None),
    };
    Ok(redis::RedisConnectionInfo {
        db,
        username,
        password,
        ..Default::default()
    })
}

/// One TCP connection to the address the net seam resolved.
async fn dial(addr: &str) -> anyhow::Result<TcpStream> {
    TcpStream::connect(addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach {addr}: {e}"))
}

/// The TLS handshake, done HERE rather than inside redis-rs.
///
/// Two reasons, and the second is the one that matters. redis-rs accepts a CA
/// only through `build_with_tls`, and then checks the name against whatever the
/// URL dialled — through a tunnel or a proxy that is a loopback port of this
/// machine's own, which no server certificate names. And this way Redis is
/// encrypted by the SAME rustls configuration as every other driver in this
/// engine (`crate::tls`), instead of by a URL fragment and a cargo feature of
/// its own: one place decides what each mode means, down to `verify_ca`
/// checking the chain and not the name.
async fn tls_stream(
    tls: &TlsSpec,
    addr: &str,
    name: &str,
) -> anyhow::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let config = crate::tls::client_config(tls)?;
    let server = rustls::pki_types::ServerName::try_from(name.to_string())
        .map_err(|_| anyhow::anyhow!("'{name}' is not a name a certificate can be checked against"))?;
    tokio_rustls::TlsConnector::from(config)
        .connect(server, dial(addr).await?)
        .await
        .map_err(|e| anyhow::anyhow!("redis tls handshake failed: {e}"))
}

/// A multiplexed connection over a stream this driver opened, with the task
/// that pumps it. The AUTH and the SELECT are redis-rs's, out of `info`.
async fn multiplex<S>(
    info: &redis::RedisConnectionInfo,
    stream: S,
) -> anyhow::Result<(MultiplexedConnection, tokio::task::JoinHandle<()>)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (con, driver) = MultiplexedConnection::new(info, stream)
        .await
        .map_err(|e| anyhow::anyhow!("redis connect failed: {e}"))?;
    Ok((con, tokio::spawn(driver)))
}

#[async_trait]
impl Driver for RedisDriver {
    fn meta(&self) -> &'static DriverMeta {
        &META
    }

    /// Redis destroys things with a verb, so the verb is the whole rule. The
    /// SQL default would pass every one of these without a word.
    fn classify(&self, statement: &str) -> Option<String> {
        let mut words = statement.split_whitespace();
        let verb = words.next()?.to_ascii_uppercase();
        let rest: Vec<&str> = words.collect();
        match verb.as_str() {
            "FLUSHALL" => Some("FLUSHALL empties EVERY database on this server".into()),
            "FLUSHDB" => Some("FLUSHDB empties the current database".into()),
            "DEL" | "UNLINK" => Some(format!(
                "{verb} removes {}",
                if rest.is_empty() {
                    "the named keys".to_string()
                } else {
                    format!("{} key(s): {}", rest.len(), rest.join(" "))
                }
            )),
            "RENAME" => Some("RENAME overwrites the destination key if it exists".into()),
            "CONFIG" if rest.first().map(|w| w.eq_ignore_ascii_case("set")).unwrap_or(false) => {
                Some("CONFIG SET changes the server's configuration".into())
            }
            "SHUTDOWN" => Some("SHUTDOWN stops the server".into()),
            "SCRIPT" if rest.first().map(|w| w.eq_ignore_ascii_case("flush")).unwrap_or(false) => {
                Some("SCRIPT FLUSH discards every cached script".into())
            }
            _ => None,
        }
    }

    async fn connect(&self, spec: &ConnSpec, net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        let host = spec.param("host")?;
        let port = spec.port(6379);
        let addr = net.resolve(host, port).await?;
        let info = connection_info(spec).await?;
        let tls = &spec.tls;

        if !tls.wanted() {
            let (con, task) = multiplex(&info, dial(&addr).await?).await?;
            return Ok(Box::new(RedisConnection {
                con,
                task,
                encrypted: false,
            }));
        }
        // The name on the certificate is the SERVER's — `host`, what somebody
        // typed — and not `addr`, which through a tunnel or a proxy is a
        // loopback port here. Saying both is what lets a tunnelled link be
        // verified at all.
        let attempt = async {
            let stream = tls_stream(tls, &addr, host).await?;
            multiplex(&info, stream).await
        }
        .await;
        match attempt {
            Ok((con, task)) => Ok(Box::new(RedisConnection {
                con,
                task,
                encrypted: true,
            })),
            Err(e) if tls.required() => Err(anyhow::anyhow!("redis: TLS is required but the connection failed: {e}")),
            // Prefer: a server with no TLS is still a server. The link comes up
            // in the clear and `encrypted` says so.
            Err(_) => {
                let (con, task) = multiplex(&info, dial(&addr).await?).await?;
                Ok(Box::new(RedisConnection {
                    con,
                    task,
                    encrypted: false,
                }))
            }
        }
    }
}

/// A live Redis connection (the multiplexed connection is cheaply cloned per call).
struct RedisConnection {
    con: MultiplexedConnection,
    /// The task pumping the socket. It belongs to this connection: left
    /// running after `close` it would outlive what it is reading for.
    task: tokio::task::JoinHandle<()>,
    encrypted: bool,
}

/// A Redis reply value → JSON (for nested arrays/maps rendered in one cell).
fn rvalue_to_json(v: &RValue) -> serde_json::Value {
    match v {
        RValue::Nil => serde_json::Value::Null,
        RValue::Int(i) => json!(i),
        RValue::Double(d) => json!(d),
        RValue::Boolean(b) => json!(b),
        RValue::BulkString(b) => json!(String::from_utf8_lossy(b)),
        RValue::SimpleString(s) => json!(s),
        RValue::Okay => json!("OK"),
        RValue::Array(a) | RValue::Set(a) => serde_json::Value::Array(a.iter().map(rvalue_to_json).collect()),
        RValue::Map(m) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in m {
                obj.insert(rvalue_to_json(k).to_string(), rvalue_to_json(val));
            }
            serde_json::Value::Object(obj)
        }
        other => json!(format!("{other:?}")),
    }
}

/// A scalar Redis reply → cell.
fn scalar(v: &RValue) -> Value {
    match v {
        RValue::Nil => Value::Null,
        RValue::Int(i) => Value::Int(*i),
        RValue::Double(d) => Value::Float(*d),
        RValue::Boolean(b) => Value::Bool(*b),
        RValue::Okay => Value::Text("OK".to_string()),
        RValue::SimpleString(s) => Value::Text(s.clone()),
        RValue::BulkString(b) => match std::str::from_utf8(b) {
            Ok(s) => Value::Text(s.to_string()),
            Err(_) => Value::Bytes {
                b64: base64::engine::general_purpose::STANDARD.encode(b),
                len: b.len(),
            },
        },
        nested => Value::Json(rvalue_to_json(nested)),
    }
}

/// What ONE row of a key looks like, and what addresses it.
///
/// This is the whole of Redis editing. Everywhere else a row is a row and a
/// table says what its key is; here the shape depends on the KIND of the key,
/// and the kind is what `structure` already labels every node with. A hash's
/// row is a field and a value, and `HSET key field value` writes it. A list's
/// row is an index, and `LSET` writes it. A set's row IS its member, so there
/// is nothing to change about it and only SREM to do to it.
///
/// Declared once, read three times — by `columns`, by the rendering of a
/// browse, and by the write — so the grid cannot show one shape while the
/// write expects another.
struct Shape {
    /// The identifying column: what `plan.key` must carry.
    key: &'static str,
    /// The column that can be changed, if any. A set member cannot.
    value: Option<&'static str>,
    /// The command that changes it: `HSET`, `LSET`, `SET`, `ZADD`.
    write: Option<&'static str>,
    /// The command that removes the row: `HDEL`, `SREM`, `ZREM`, `XDEL`, `DEL`.
    remove: Option<&'static str>,
    /// Why a list cannot have a row removed, or a stream entry changed. Said
    /// instead of doing, and said in the driver because it is a fact about
    /// Redis.
    note: &'static str,
}

/// The shape of a key of this kind, or `None` for a kind this driver does not
/// know how to address a row of.
fn shape_of(kind: &str) -> Option<Shape> {
    match kind {
        // One key, one value. The key column is the address, and it is the
        // only row there is.
        "string" => Some(Shape {
            key: "key",
            value: Some("value"),
            write: Some("SET"),
            remove: Some("DEL"),
            note: "",
        }),
        "hash" => Some(Shape {
            key: "field",
            value: Some("value"),
            write: Some("HSET"),
            remove: Some("HDEL"),
            note: "",
        }),
        // LSET writes by index. Nothing REMOVES by index: LREM removes by
        // value, which is a different row from the one that was pointed at.
        "list" => Some(Shape {
            key: "index",
            value: Some("value"),
            write: Some("LSET"),
            remove: None,
            note: "a list element is removed by VALUE (LREM), not by position, so removing this row \
                   would mean removing every element equal to it",
        }),
        // The member IS the row. Changing it would be removing one member and
        // adding another, which is not an edit of this one.
        "set" => Some(Shape {
            key: "member",
            value: None,
            write: None,
            remove: Some("SREM"),
            note: "a set member is its own value: there is nothing else about it to change",
        }),
        "zset" => Some(Shape {
            key: "member",
            value: Some("score"),
            write: Some("ZADD"),
            remove: Some("ZREM"),
            note: "",
        }),
        // An entry already written cannot be rewritten; it can be dropped.
        "stream" => Some(Shape {
            key: "id",
            value: None,
            write: None,
            remove: Some("XDEL"),
            note: "a stream entry is immutable once written \u{2014} it can be dropped (XDEL), not edited",
        }),
        _ => None,
    }
}

/// The columns a browse of a key of this kind produces, as the grid sees them.
fn shape_columns(kind: &str) -> Vec<Column> {
    match shape_of(kind) {
        Some(sh) => std::iter::once(sh.key)
            .chain(sh.value)
            .map(|name| Column {
                name: name.to_string(),
                type_name: String::new(),
            })
            .collect(),
        None => Vec::new(),
    }
}

/// One argument of a command, out of a cell the client sent.
///
/// It goes out as an ARGUMENT, not as text in a command line, so there is
/// nothing for it to be injected into.
fn arg_of(v: &serde_json::Value) -> String {
    match super::rowsql::bind_of(&crate::spec::value_from_json(v)) {
        super::rowsql::Bind::Null => String::new(),
        super::rowsql::Bind::Text(s) => s,
        super::rowsql::Bind::Bytes(raw) => String::from_utf8_lossy(&raw).into_owned(),
    }
}

/// The command a row write becomes, and the line to SHOW for it.
///
/// The shown line carries `?` where a value goes: an argument never appears in
/// the audit log, which is the same promise the SQL drivers keep by binding
/// their parameters.
fn row_command(kind: &str, key: &str, plan: &RowPlan) -> anyhow::Result<(Vec<String>, String)> {
    let sh = shape_of(kind).ok_or_else(|| {
        anyhow::anyhow!("a {kind} key has no rows this driver knows how to address")
    })?;
    let (_, addr) = plan
        .key
        .iter()
        .find(|(c, _)| c == sh.key)
        .map(|(c, v)| (c.clone(), arg_of(v)))
        .ok_or_else(|| anyhow::anyhow!("that row is addressed by {}, which was not sent", sh.key))?;

    if plan.delete {
        let verb = sh.remove.ok_or_else(|| anyhow::anyhow!("{}", sh.note))?;
        // DEL takes the key itself; everything else takes the key and then
        // what inside it to remove.
        let argv = if verb == "DEL" {
            vec![verb.to_string(), key.to_string()]
        } else {
            vec![verb.to_string(), key.to_string(), addr.clone()]
        };
        let shown = argv.join(" ");
        return Ok((argv, shown));
    }

    let verb = sh.write.ok_or_else(|| anyhow::anyhow!("{}", sh.note))?;
    let column = sh
        .value
        .ok_or_else(|| anyhow::anyhow!("{}", sh.note))?;
    let value = plan
        .set
        .iter()
        .find(|(c, _)| c == column)
        .map(|(_, v)| arg_of(v))
        .ok_or_else(|| anyhow::anyhow!("{column} is the only thing about this row that can change"))?;

    let (argv, shown) = match verb {
        // SET key value
        "SET" => (
            vec![verb.to_string(), key.to_string(), value],
            format!("SET {key} ?"),
        ),
        // HSET key field value / LSET key index value
        "HSET" | "LSET" => (
            vec![verb.to_string(), key.to_string(), addr.clone(), value],
            format!("{verb} {key} {addr} ?"),
        ),
        // ZADD key score member — the score is the value and the member is the
        // address, so this one is the other way round.
        "ZADD" => (
            vec![verb.to_string(), key.to_string(), value, addr.clone()],
            format!("ZADD {key} ? {addr}"),
        ),
        other => anyhow::bail!("{other} is not a write this driver builds"),
    };
    Ok((argv, shown))
}

/// Tokenise a Redis command line (double-quote aware).
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for ch in line.trim().chars() {
        match ch {
            '"' => in_q = !in_q,
            c if c.is_whitespace() && !in_q => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl RedisConnection {
    /// What kind of key this is, in the server's own word — the same word
    /// `structure` labels the node with, and what every shape follows from.
    async fn kind_of(&self, key: &str) -> anyhow::Result<String> {
        match self.command(&["TYPE".to_string(), key.to_string()]).await? {
            RValue::SimpleString(s) => Ok(s),
            RValue::BulkString(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
            RValue::Nil => anyhow::bail!("there is no key called '{key}' any more"),
            other => anyhow::bail!("the server did not say what '{key}' is: {other:?}"),
        }
    }

    async fn command(&self, tokens: &[String]) -> anyhow::Result<RValue> {
        if tokens.is_empty() {
            return Err(anyhow::anyhow!("empty command"));
        }
        let mut c = redis::cmd(&tokens[0]);
        for a in &tokens[1..] {
            c.arg(a);
        }
        let mut con = self.con.clone();
        c.query_async(&mut con).await.map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// Render the reply to a command that BROWSES one key, so that every row says
/// which value it is.
///
/// The generic rendering below cannot do this: `LRANGE` comes back as a bare
/// array, and an array of values with no positions is a grid in which no row
/// can be pointed at — which is why this driver could show a key and not edit
/// one. The command is what says which key and which kind, and the command is
/// right here, so the answer is rendered in the shape `shape_of` declares.
///
/// Anything else — `KEYS *`, `INFO`, a command somebody typed — falls through
/// to `render`, unchanged.
fn render_browse(tokens: &[String], v: RValue) -> (Vec<Column>, Vec<Vec<Value>>) {
    let verb = match tokens.first() {
        Some(w) => w.to_ascii_uppercase(),
        None => return render(v),
    };
    let key = match tokens.get(1) {
        Some(k) => k.clone(),
        None => return render(v),
    };
    let flat = |v: &RValue| -> Vec<Value> {
        match v {
            RValue::Array(a) | RValue::Set(a) => a.iter().map(scalar).collect(),
            RValue::Map(m) => m.iter().flat_map(|(k, val)| [scalar(k), scalar(val)]).collect(),
            one => vec![scalar(one)],
        }
    };
    let pairs = |cells: Vec<Value>| -> Vec<Vec<Value>> {
        cells.chunks(2).map(|c| c.to_vec()).collect()
    };
    match verb.as_str() {
        // GET key → one row, and the key column is the address of it.
        "GET" => (
            shape_columns("string"),
            vec![vec![Value::Text(key), flat(&v).into_iter().next().unwrap_or(Value::Null)]],
        ),
        // HGETALL → field, value. RESP2 sends a flat array and RESP3 a map;
        // both arrive here as a flat list of cells.
        "HGETALL" => (shape_columns("hash"), pairs(flat(&v))),
        // LRANGE key start stop → index, value. The index is absolute, which
        // is what LSET takes — so a NEGATIVE start, where this cannot be
        // worked out, is rendered as a plain list rather than as positions
        // that would be wrong.
        "LRANGE" => match tokens.get(2).and_then(|s| s.parse::<i64>().ok()) {
            Some(start) if start >= 0 => (
                shape_columns("list"),
                flat(&v)
                    .into_iter()
                    .enumerate()
                    .map(|(i, cell)| vec![Value::Int(start + i as i64), cell])
                    .collect(),
            ),
            _ => render(v),
        },
        "SMEMBERS" => (
            shape_columns("set"),
            flat(&v).into_iter().map(|m| vec![m]).collect(),
        ),
        // ZRANGE … WITHSCORES → member, score. Without the scores there is
        // nothing to edit, so it stays a list of members.
        "ZRANGE" | "ZRANGEBYSCORE" | "ZREVRANGE" => {
            if tokens.iter().any(|t| t.eq_ignore_ascii_case("WITHSCORES")) {
                (shape_columns("zset"), pairs(flat(&v)))
            } else {
                (
                    vec![Column {
                        name: "member".to_string(),
                        type_name: String::new(),
                    }],
                    flat(&v).into_iter().map(|m| vec![m]).collect(),
                )
            }
        }
        // XRANGE → id, fields. The id is what XDEL takes; the fields are
        // shown as they are, because a stream entry cannot be edited and
        // pretending otherwise would be a column that refuses every write.
        "XRANGE" | "XREVRANGE" => {
            let entries = match &v {
                RValue::Array(a) => a.clone(),
                _ => return render(v),
            };
            let columns = vec![
                Column {
                    name: "id".to_string(),
                    type_name: String::new(),
                },
                Column {
                    name: "fields".to_string(),
                    type_name: String::new(),
                },
            ];
            let rows = entries
                .iter()
                .map(|e| match e {
                    RValue::Array(pair) if pair.len() == 2 => {
                        vec![scalar(&pair[0]), Value::Json(rvalue_to_json(&pair[1]))]
                    }
                    other => vec![scalar(other), Value::Null],
                })
                .collect();
            (columns, rows)
        }
        _ => render(v),
    }
}

/// Render a Redis reply into (columns, rows).
fn render(v: RValue) -> (Vec<Column>, Vec<Vec<Value>>) {
    let value_col = vec![Column {
        name: "value".to_string(),
        type_name: String::new(),
    }];
    match v {
        RValue::Array(a) | RValue::Set(a) => (value_col, a.iter().map(|e| vec![scalar(e)]).collect()),
        RValue::Map(m) => {
            let cols = vec![
                Column {
                    name: "key".to_string(),
                    type_name: String::new(),
                },
                Column {
                    name: "value".to_string(),
                    type_name: String::new(),
                },
            ];
            let rows = m.iter().map(|(k, val)| vec![scalar(k), scalar(val)]).collect();
            (cols, rows)
        }
        scalar_v => (value_col, vec![vec![scalar(&scalar_v)]]),
    }
}

#[async_trait]
impl Connection for RedisConnection {
    async fn databases(&mut self) -> anyhow::Result<Vec<String>> {
        // CONFIG GET databases → ["databases", "16"].
        let reply = self
            .command(&["CONFIG".into(), "GET".into(), "databases".into()])
            .await
            .ok();
        let count = match reply {
            Some(RValue::Array(a)) => a
                .get(1)
                .and_then(|v| match v {
                    RValue::BulkString(b) => std::str::from_utf8(b).ok().and_then(|s| s.parse::<usize>().ok()),
                    RValue::Int(i) => Some(*i as usize),
                    _ => None,
                })
                .unwrap_or(16),
            _ => 16,
        };
        Ok((0..count).map(|i| i.to_string()).collect())
    }

    async fn switch_database(&mut self, db: &str) -> anyhow::Result<()> {
        self.command(&["SELECT".into(), db.to_string()]).await?;
        Ok(())
    }

    async fn structure(&mut self) -> anyhow::Result<Vec<Node>> {
        // One bounded SCAN pass → keys, each labelled by its Redis type.
        let reply = self
            .command(&["SCAN".into(), "0".into(), "COUNT".into(), "300".into()])
            .await?;
        let keys: Vec<String> = match reply {
            RValue::Array(a) => match a.get(1) {
                Some(RValue::Array(ks)) => ks
                    .iter()
                    .filter_map(|k| match k {
                        RValue::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let mut children = Vec::with_capacity(keys.len());
        for key in keys {
            let t = self.command(&["TYPE".into(), key.clone()]).await.ok();
            let kind = match t {
                Some(RValue::SimpleString(s)) => s,
                _ => "key".to_string(),
            };
            children.push(Node {
                name: key,
                kind,
                schema: Some("keyspace".to_string()),
                children: Vec::new(),
            });
        }
        Ok(vec![Node {
            name: "keyspace".to_string(),
            kind: "schema".to_string(),
            schema: None,
            children,
        }])
    }

    /// What one row of this key looks like, and which column addresses it.
    ///
    /// A key/value store has no schema, but a KEY has a kind, and the kind is
    /// what decides both — so this asks the server what the key is and answers
    /// from `shape_of`. Returning nothing here (as this did) is what made the
    /// engine refuse every edit with "has no primary key".
    async fn columns(&mut self, obj: &ObjRef) -> anyhow::Result<Vec<TableColumn>> {
        let kind = self.kind_of(&obj.name).await?;
        let Some(sh) = shape_of(&kind) else {
            return Ok(Vec::new());
        };
        let mut out = vec![TableColumn {
            name: sh.key.to_string(),
            type_name: kind.clone(),
            primary: true,
            references: None,
        }];
        if let Some(value) = sh.value {
            out.push(TableColumn {
                name: value.to_string(),
                type_name: kind,
                primary: false,
                references: None,
            });
        }
        Ok(out)
    }

    /// `CLIENT LIST`, parsed into columns.
    ///
    /// The reply is one line per client, each line `field=value` pairs
    /// separated by spaces. Only the fields worth reading at a glance are kept
    /// — all of them would be twenty columns and the useful four are the same
    /// four every time.
    ///
    /// UNVERIFIED against a server, like `metrics` below: written from the
    /// documented shape of the reply.
    async fn inspect(
        &mut self,
        name: &str,
    ) -> anyhow::Result<crate::driver::Inspected> {
        if name != "clients" {
            anyhow::bail!("this driver has no live view called '{name}'");
        }
        let reply = self.command(&["CLIENT".to_string(), "LIST".to_string()]).await?;
        let text = match reply {
            RValue::BulkString(b) => String::from_utf8_lossy(&b).to_string(),
            RValue::SimpleString(s) => s,
            other => format!("{other:?}"),
        };
        // `addr` FIRST, because that is the key `act` is handed: Redis kills a
        // client by its address, not by the numeric id.
        let want = ["addr", "id", "name", "age", "idle", "db", "cmd"];
        let columns = want
            .iter()
            .map(|c| Column { name: c.to_string(), type_name: String::new() })
            .collect();
        let mut rows = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let mut got: std::collections::BTreeMap<&str, &str> = Default::default();
            for pair in line.split(' ') {
                if let Some((k, v)) = pair.split_once('=') {
                    got.insert(k, v);
                }
            }
            rows.push(
                want.iter()
                    .map(|k| match got.get(k) {
                        // A number stays a number, so the grid can sort by it:
                        // an age of "9" and an age of "100" sort the wrong way
                        // round as text.
                        Some(v) => match v.parse::<i64>() {
                            Ok(n) if matches!(*k, "id" | "age" | "idle" | "db") => Value::Int(n),
                            _ => Value::Text(v.to_string()),
                        },
                        None => Value::Null,
                    })
                    .collect(),
            );
        }
        // Every row of CLIENT LIST is a client.
        Ok(crate::driver::Inspected::all(columns, rows))
    }

    /// Disconnect a client, by its address.
    ///
    /// `CLIENT KILL ADDR <addr>` — the address, because that is the form every
    /// Redis since 2.8.12 understands, where `ID` arrived later. The address
    /// comes back from a row this server itself produced and goes out as an
    /// ARGUMENT rather than as text in a command line, so there is nothing for
    /// it to be injected into.
    async fn act(&mut self, name: &str, key: &str) -> anyhow::Result<String> {
        if name != "clients" {
            anyhow::bail!("nothing can be done to a row of '{name}'");
        }
        let addr = key.trim();
        if addr.is_empty() {
            anyhow::bail!("that row carries no address to kill");
        }
        let reply = self
            .command(&[
                "CLIENT".to_string(),
                "KILL".to_string(),
                "ADDR".to_string(),
                addr.to_string(),
            ])
            .await?;
        // It answers with a COUNT of clients killed, and zero is not a
        // failure — the client had already gone — but it is not a success
        // either, and saying so is the difference.
        let killed = match reply {
            RValue::Int(n) => n,
            RValue::SimpleString(ref s) if s == "OK" => 1,
            _ => 0,
        };
        Ok(if killed > 0 {
            format!("disconnected {addr}")
        } else {
            format!("{addr} was not there to disconnect")
        })
    }

    async fn metrics(&mut self) -> anyhow::Result<Vec<crate::spec::Metric>> {
        use crate::spec::Metric;
        // Redis says everything about itself through one command, as `field:value`
        // lines under `# Section` headings.
        //
        // UNVERIFIED: written from the documented shape of INFO, not run against
        // a server — there is none here, and the one in reach is somebody's.
        let reply = self.command(&["INFO".to_string()]).await?;
        let text = match render(reply) {
            (_, rows) => rows
                .into_iter()
                .flatten()
                .map(|v| match v {
                    Value::Text(s) => s,
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        // Only the fields worth a panel. INFO returns well over a hundred, and a
        // panel of a hundred numbers is a panel nobody reads.
        const WANTED: &[(&str, &str, &str)] = &[
            ("storage", "memory used", "used_memory_human"),
            ("storage", "memory peak", "used_memory_peak_human"),
            ("storage", "keys", "db0"),
            ("activity", "clients", "connected_clients"),
            ("activity", "commands processed", "total_commands_processed"),
            ("activity", "ops per second", "instantaneous_ops_per_sec"),
            ("activity", "keyspace hits", "keyspace_hits"),
            ("activity", "keyspace misses", "keyspace_misses"),
            ("activity", "evicted keys", "evicted_keys"),
            ("settings", "version", "redis_version"),
            ("settings", "mode", "redis_mode"),
            ("settings", "role", "role"),
            ("settings", "uptime (days)", "uptime_in_days"),
        ];
        let mut out = Vec::new();
        for (group, name, field) in WANTED {
            if let Some(line) = text.lines().find(|l| l.starts_with(&format!("{field}:"))) {
                let value = line.splitn(2, ':').nth(1).unwrap_or("").trim().to_string();
                if !value.is_empty() {
                    out.push(Metric { group, name, value });
                }
            }
        }
        if out.is_empty() {
            anyhow::bail!("INFO returned nothing this panel knows how to read");
        }
        Ok(out)
    }

    async fn plan_row(&mut self, plan: &RowPlan) -> anyhow::Result<String> {
        let kind = self.kind_of(&plan.obj.name).await?;
        Ok(row_command(&kind, &plan.obj.name, plan)?.1)
    }

    /// Change or remove one row of a key.
    ///
    /// There is no transaction here and none is needed: every command this
    /// builds addresses exactly ONE thing by name — a field, an index, a
    /// member, the key itself — so it cannot touch a second row by being
    /// wrong about a WHERE. That is the same promise the SQL drivers keep with
    /// a transaction, kept instead by the shape of the command.
    async fn write_row(&mut self, plan: &RowPlan) -> anyhow::Result<RowOutcome> {
        let kind = self.kind_of(&plan.obj.name).await?;
        let (argv, statement) = row_command(&kind, &plan.obj.name, plan)?;
        let reply = self.command(&argv).await?;
        // A removal says HOW MANY it removed, and zero means the row was
        // already gone — which is not a failure and is not a success either.
        let affected = match reply {
            RValue::Int(n) if plan.delete => {
                if n == 0 {
                    anyhow::bail!("that row was not there any more \u{2014} nothing was removed");
                }
                n.max(0) as u64
            }
            // HSET answers 0 for a field it replaced and 1 for one it created;
            // both wrote the value that was asked for, so both are one row.
            RValue::Int(_) | RValue::Okay => 1,
            RValue::SimpleString(ref s) if s == "OK" => 1,
            other => anyhow::bail!("the server answered something this driver cannot read: {other:?}"),
        };
        Ok(RowOutcome { statement, affected })
    }

    async fn execute(&mut self, stmt: &str) -> anyhow::Result<Box<dyn ResultStream>> {
        let tokens = tokenize(stmt);
        // Redis has no parse stage of its own — the server answers — so a
        // statement in the wrong language comes back as the server's complaint.
        // "unknown command" is the server saying exactly that, and it is the one
        // case worth answering with what this driver does take. Any other error
        // is a real Redis error and is passed through untouched.
        let reply = self.command(&tokens).await.map_err(|e| {
            let msg = e.to_string();
            if msg.to_ascii_lowercase().contains("unknown command") {
                anyhow::anyhow!("{msg}\nRedis takes a command, for example: {}", META.hint)
            } else {
                e
            }
        })?;
        let (columns, rows) = render_browse(&tokens, reply);
        Ok(Box::new(super::buffered::BufferedStream::new(columns, rows, None)))
    }

    fn encrypted(&self) -> bool {
        self.encrypted
    }

    async fn close(self: Box<Self>) -> anyhow::Result<()> {
        self.task.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(auth: serde_json::Value, db: &str) -> ConnSpec {
        serde_json::from_value(json!({
            "driver": "redis",
            "params": { "host": "cache.example", "db": db },
            "auth": auth,
        }))
        .expect("a redis spec")
    }

    /// The reason the URL went: a password with `@` and `%` in it used to be
    /// percent-encoded into the userinfo, and one mistake there connects
    /// somebody to the wrong host with half a password. As a field it is the
    /// bytes they typed.
    #[tokio::test]
    async fn a_password_travels_as_itself() {
        let info = connection_info(&spec(json!({ "kind": "password", "user": "app", "password": "p@ss%3a#1" }), "0"))
            .await
            .expect("the secret resolves");
        assert_eq!(info.username.as_deref(), Some("app"));
        assert_eq!(info.password.as_deref(), Some("p@ss%3a#1"));
    }

    /// Redis before 6 has no users: a password alone is `AUTH <password>`, and
    /// an empty username would make it `AUTH "" <password>`, which fails.
    #[tokio::test]
    async fn a_password_without_a_user_carries_no_user() {
        let info = connection_info(&spec(json!({ "kind": "password", "user": "", "password": "only-a-password" }), "0"))
            .await
            .expect("the secret resolves");
        assert!(info.username.is_none());
        assert_eq!(info.password.as_deref(), Some("only-a-password"));
    }

    #[tokio::test]
    async fn no_auth_means_no_auth() {
        let info = connection_info(&spec(json!({ "kind": "none" }), "0"))
            .await
            .expect("nothing to resolve");
        assert!(info.username.is_none() && info.password.is_none());
    }

    /// The DB index is a number on the wire, and the form hands it over as the
    /// text somebody typed.
    #[tokio::test]
    async fn the_database_is_the_one_that_was_chosen() {
        let info = connection_info(&spec(json!({ "kind": "none" }), "7")).await.expect("ok");
        assert_eq!(info.db, 7);
        let fallback = connection_info(&spec(json!({ "kind": "none" }), "not a number"))
            .await
            .expect("ok");
        assert_eq!(fallback.db, 0, "a number nobody can read is database 0, not a refusal");
    }
}

#[cfg(test)]
mod row_tests {
    use super::*;

    fn plan(obj: &str, key: (&str, serde_json::Value), set: Vec<(&str, serde_json::Value)>, delete: bool) -> RowPlan {
        serde_json::from_value(json!({
            "obj": { "name": obj, "schema": "keyspace" },
            "key": [[key.0, key.1]],
            "set": set.into_iter().map(|(c, v)| json!([c, v])).collect::<Vec<_>>(),
            "delete": delete,
        }))
        .expect("a plan")
    }

    /// The point of the whole thing: a hash row is a field, and the command
    /// that writes it names the field and sends the value as an argument.
    #[test]
    fn a_hash_field_is_written_by_name() {
        let p = plan("user:1", ("field", json!("name")), vec![("value", json!("Ада"))], false);
        let (argv, shown) = row_command("hash", "user:1", &p).expect("a hash row can be written");
        assert_eq!(argv, vec!["HSET", "user:1", "name", "Ада"]);
        assert_eq!(shown, "HSET user:1 name ?", "the value does not appear in the log");
    }

    #[test]
    fn a_list_element_is_written_by_position() {
        let p = plan("queue", ("index", json!(3)), vec![("value", json!("x"))], false);
        let (argv, shown) = row_command("list", "queue", &p).expect("a list row can be written");
        assert_eq!(argv, vec!["LSET", "queue", "3", "x"]);
        assert_eq!(shown, "LSET queue 3 ?");
    }

    /// ZADD takes the score BEFORE the member, which is the other way round
    /// from every other write here.
    #[test]
    fn a_sorted_set_takes_its_score_first() {
        let p = plan("board", ("member", json!("ada")), vec![("score", json!(42))], false);
        let (argv, shown) = row_command("zset", "board", &p).expect("a zset row can be written");
        assert_eq!(argv, vec!["ZADD", "board", "42", "ada"]);
        assert_eq!(shown, "ZADD board ? ada");
    }

    #[test]
    fn a_string_key_is_its_own_row() {
        let p = plan("greeting", ("key", json!("greeting")), vec![("value", json!("hello"))], false);
        let (argv, _) = row_command("string", "greeting", &p).expect("a string can be written");
        assert_eq!(argv, vec!["SET", "greeting", "hello"]);
    }

    #[test]
    fn each_kind_removes_a_row_its_own_way() {
        for (kind, key, col, want) in [
            ("hash", "user:1", "field", vec!["HDEL", "user:1", "name"]),
            ("set", "tags", "member", vec!["SREM", "tags", "name"]),
            ("zset", "board", "member", vec!["ZREM", "board", "name"]),
            ("stream", "events", "id", vec!["XDEL", "events", "name"]),
            ("string", "greeting", "key", vec!["DEL", "greeting"]),
        ] {
            let p = plan(key, (col, json!("name")), vec![], true);
            let (argv, _) = row_command(kind, key, &p).unwrap_or_else(|e| panic!("{kind}: {e}"));
            assert_eq!(argv, want, "{kind}");
        }
    }

    /// What Redis cannot do is SAID, not quietly done as something else.
    #[test]
    fn what_redis_cannot_do_it_explains() {
        let by_index = plan("queue", ("index", json!(0)), vec![], true);
        let e = row_command("list", "queue", &by_index).expect_err("a list has no removal by position");
        assert!(e.to_string().contains("LREM"), "it says what Redis does instead: {e}");

        let member = plan("tags", ("member", json!("rust")), vec![("member", json!("go"))], false);
        let e = row_command("set", "tags", &member).expect_err("a set member is its own value");
        assert!(e.to_string().contains("nothing else about it"), "{e}");

        let entry = plan("events", ("id", json!("1-0")), vec![("fields", json!("{}"))], false);
        let e = row_command("stream", "events", &entry).expect_err("a stream entry is immutable");
        assert!(e.to_string().contains("immutable"), "{e}");

        let p = plan("x", ("key", json!("x")), vec![("value", json!("v"))], false);
        let e = row_command("vector", "x", &p).expect_err("an unknown kind has no shape");
        assert!(e.to_string().contains("vector"), "{e}");
    }

    /// The browse and the write have to agree about what a row is, or the grid
    /// shows one shape and the write asks for another.
    #[test]
    fn the_grid_shows_what_the_write_addresses() {
        for (kind, cmd) in [
            ("hash", vec!["HGETALL", "user:1"]),
            ("list", vec!["LRANGE", "queue", "0", "99"]),
            ("set", vec!["SMEMBERS", "tags"]),
            ("zset", vec!["ZRANGE", "board", "0", "99", "WITHSCORES"]),
            ("string", vec!["GET", "greeting"]),
        ] {
            let tokens: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
            let (columns, _) = render_browse(&tokens, RValue::Array(vec![]));
            let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            let want: Vec<String> = shape_columns(kind).iter().map(|c| c.name.clone()).collect();
            assert_eq!(names, want, "{kind}: the browse and the shape disagree");
        }
    }

    /// LSET takes an ABSOLUTE position, so a browse that cannot work one out
    /// does not invent one.
    #[test]
    fn a_list_browsed_from_the_end_shows_no_positions() {
        let from_start: Vec<String> = ["LRANGE", "q", "10", "20"].iter().map(|s| s.to_string()).collect();
        let reply = RValue::Array(vec![RValue::SimpleString("a".into()), RValue::SimpleString("b".into())]);
        let (_, rows) = render_browse(&from_start, reply.clone());
        assert!(matches!(rows[0][0], Value::Int(10)), "the first row is at 10");
        assert!(matches!(rows[1][0], Value::Int(11)));

        let from_end: Vec<String> = ["LRANGE", "q", "-5", "-1"].iter().map(|s| s.to_string()).collect();
        let (columns, _) = render_browse(&from_end, reply);
        assert_eq!(columns.len(), 1, "no index column, because the position is not knowable");
    }
}
