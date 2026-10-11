// lode/drivers/postgres: the PostgreSQL driver (also the CockroachDB
// registry alias — same wire protocol, different defaults/kind).
//
// Results use the Postgres SIMPLE (text) query protocol: the server stringifies
// every value — including numeric, json, arrays and custom types — so the grid
// renders any column with no per-type FromSql handling. The whole result is
// buffered in the daemon and paged out to Neovim a page at a time (so the editor
// only ever holds one page). TLS/rustls and the SSH tunnel land in later phases;
// this slice dials plaintext through the NetContext seam.

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tokio_postgres::{Client, Config, NoTls, SimpleQueryMessage};

use crate::driver::{CancelHandle, Connection, Driver, ResultStream};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, AuthSpec, Caps, Column, ConnSpec, DriverMeta, Index, Node, ObjRef, ParamSpec, ParamType, TableColumn,
    Value,
};

// ── driver metadata ──────────────────────────────────────────────────────────

/// PostgreSQL's list is the shared one plus the case-insensitive match it is
/// alone in having as an operator.
const PG_BUILDER_OPS: &[crate::spec::BuilderOp] = &[
    crate::spec::BuilderOp { label: "=", fragment: "{col} = {value}" },
    crate::spec::BuilderOp { label: "<>", fragment: "{col} <> {value}" },
    crate::spec::BuilderOp { label: "<", fragment: "{col} < {value}" },
    crate::spec::BuilderOp { label: "<=", fragment: "{col} <= {value}" },
    crate::spec::BuilderOp { label: ">", fragment: "{col} > {value}" },
    crate::spec::BuilderOp { label: ">=", fragment: "{col} >= {value}" },
    crate::spec::BuilderOp { label: "LIKE", fragment: "{col} LIKE {value}" },
    crate::spec::BuilderOp { label: "ILIKE", fragment: "{col} ILIKE {value}" },
    crate::spec::BuilderOp { label: "IN", fragment: "{col} IN ({value})" },
    crate::spec::BuilderOp { label: "IS NULL", fragment: "{col} IS NULL" },
    crate::spec::BuilderOp { label: "IS NOT NULL", fragment: "{col} IS NOT NULL" },
];

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
        default: Some("5432"),
    },
    ParamSpec {
        key: "database",
        label: "Database",
        kind: ParamType::String,
        required: true,
        secret: false,
        default: Some("postgres"),
    },
];

const AUTH: &[AuthKind] = &[AuthKind::None, AuthKind::Password, AuthKind::ClientCert];

const PG_META: DriverMeta = DriverMeta {
    kind: "postgres",
    display: "PostgreSQL",
    default_port: Some(5432),
    params: PARAMS,
    auth: AUTH,
    caps: Caps {
        sql: true,
        schemas: true,
        cancel: true,
        tls: true,
        tunnel: true,
        multi_db: true,
        kv: false,
        indexes: true, // pg_index + pg_attribute
        ddl: false,    // postgres has NO server-side CREATE TABLE (pg_dump is a client) — see the trait doc
        edit: false,
        // pg_stat_database / pg_stat_activity — reads. Unverified here.
        metrics: true,
    },
    hint: "SELECT * FROM table LIMIT 100",
    browse: "SELECT {columns} FROM {qualified} {clauses} LIMIT {limit};",
    browse_by_kind: &[],
    query_form: &[],
    query_template: "",
    browse_form: crate::spec::SQL_BROWSE_FORM,
    time_filter: crate::spec::SQL_TIME_FILTER,
    builder_ops: PG_BUILDER_OPS,
    inspectors: PG_INSPECTORS,
};

/// What a PostgreSQL server will say about itself, live.
///
/// Both are reads of the statistics views. `activity` can also TERMINATE a
/// session, which is `pg_terminate_backend` and nothing else — the one
/// destructive thing these views are for.
const PG_INSPECTORS: &[crate::spec::Inspector] = &[
    crate::spec::Inspector {
        name: "activity",
        label: "Who is connected",
        act: "terminate",
    },
    crate::spec::Inspector { name: "locks", label: "What is locked", act: "" },
];

#[cfg(feature = "cockroachdb")]
const CRDB_META: DriverMeta = DriverMeta {
    kind: "cockroachdb",
    display: "CockroachDB",
    default_port: Some(26257),
    params: PARAMS,
    auth: AUTH,
    caps: PG_META.caps,
    hint: "SELECT * FROM table LIMIT 100",
    browse: "SELECT {columns} FROM {qualified} {clauses} LIMIT {limit};",
    browse_by_kind: &[],
    query_form: &[],
    query_template: "",
    browse_form: crate::spec::SQL_BROWSE_FORM,
    time_filter: crate::spec::SQL_TIME_FILTER,
    builder_ops: PG_BUILDER_OPS,
    // The same wire, so the same statistics views: this IS the postgres driver
    // underneath, and a view that works through it works through the alias.
    inspectors: PG_INSPECTORS,
};

/// Amazon Redshift, which speaks the PostgreSQL wire protocol.
///
/// A registry alias and nothing more, for the same reason CockroachDB is one:
/// the wire is the same, so a second implementation would be the same code with
/// a second set of bugs. What differs is the metadata — the port, the name, and
/// a hint that says what Redshift is actually for.
///
/// NOT VERIFIED against a real Redshift cluster: there is none here and no
/// network. What IS verified is that the alias registers, resolves, and carries
/// its own metadata — which is the whole of what an alias adds. The wire
/// underneath is the postgres driver, which is the one that was already there.
#[cfg(feature = "redshift")]
const REDSHIFT_META: DriverMeta = DriverMeta {
    kind: "redshift",
    display: "Amazon Redshift",
    default_port: Some(5439),
    params: PARAMS,
    auth: AUTH,
    caps: Caps {
        // Redshift has no per-object CREATE to ask for the way PostgreSQL does
        // (`pg_get_tabledef` is not there), so the drawer does not offer a
        // helper that would dead-end.
        ddl: false,
        ..PG_META.caps
    },
    hint: "SELECT * FROM table LIMIT 100",
    browse: "SELECT {columns} FROM {qualified} {clauses} LIMIT {limit};",
    browse_by_kind: &[],
    query_form: &[],
    query_template: "",
    browse_form: crate::spec::SQL_BROWSE_FORM,
    time_filter: crate::spec::SQL_TIME_FILTER,
    builder_ops: PG_BUILDER_OPS,
    // The same wire, so the same statistics views: this IS the postgres driver
    // underneath, and a view that works through it works through the alias.
    inspectors: PG_INSPECTORS,
};

/// The Postgres-wire driver. One impl serves the `postgres`, `cockroachdb` and
/// `redshift` registry kinds (they differ only in metadata/defaults).
pub struct PostgresDriver {
    meta: &'static DriverMeta,
}

impl PostgresDriver {
    pub fn postgres() -> Self {
        Self { meta: &PG_META }
    }

    #[cfg(feature = "cockroachdb")]
    pub fn cockroachdb() -> Self {
        Self { meta: &CRDB_META }
    }

    #[cfg(feature = "redshift")]
    pub fn redshift() -> Self {
        Self { meta: &REDSHIFT_META }
    }
}

/// Turn a tokio-postgres error into a useful message. Its own Display is a terse
/// "db error"/"error connecting…"; the actionable text (the server's message,
/// SQLSTATE, and any DETAIL/HINT) lives in the wrapped DbError, so surface that.
fn pg_error(e: tokio_postgres::Error) -> anyhow::Error {
    if let Some(db) = e.as_db_error() {
        let mut msg = format!("{}: {} [{}]", db.severity(), db.message(), db.code().code());
        if let Some(detail) = db.detail() {
            msg.push_str(&format!(" — {detail}"));
        }
        if let Some(hint) = db.hint() {
            msg.push_str(&format!(" (hint: {hint})"));
        }
        anyhow::anyhow!(msg)
    } else {
        anyhow::anyhow!("{e}")
    }
}

/// Derive (user, password) from a spec's auth, resolving the password template.
async fn credentials(spec: &ConnSpec) -> anyhow::Result<(String, String)> {
    match &spec.auth {
        AuthSpec::Password { user, password } => {
            let u = if user.is_empty() {
                spec.param_opt("user").unwrap_or("postgres").to_string()
            } else {
                user.clone()
            };
            Ok((u, password.resolve().await?))
        }
        AuthSpec::ClientCert { user, .. } => {
            // TLS mutual-auth cert flow lands with rustls; the username still applies.
            let u = if user.is_empty() {
                spec.param_opt("user").unwrap_or("postgres").to_string()
            } else {
                user.clone()
            };
            Ok((u, String::new()))
        }
        _ => Ok((spec.param_opt("user").unwrap_or("postgres").to_string(), String::new())),
    }
}

/// Build a tokio-postgres Config for `spec`, dialing through `net` (so a tunnel's
/// local endpoint is honoured once tunnelling is wired).
async fn build_config(spec: &ConnSpec, net: &NetContext) -> anyhow::Result<Config> {
    let host = spec.param("host")?;
    let port = spec.port(5432);
    let db = spec.param("database")?;
    let (user, password) = credentials(spec).await?;

    // Resolve the dial address through the net seam (identity without a tunnel).
    let addr = net.resolve(host, port).await?;
    let (rhost, rport) = addr
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("net resolved a malformed address"))?;

    let mut cfg = Config::new();
    cfg.host(rhost);
    cfg.port(rport.parse().unwrap_or(port));
    cfg.user(&user);
    cfg.dbname(db);
    if !password.is_empty() {
        cfg.password(password);
    }
    Ok(cfg)
}

#[async_trait]
impl Driver for PostgresDriver {
    fn meta(&self) -> &'static DriverMeta {
        self.meta
    }

    async fn connect(&self, spec: &ConnSpec, net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        let cfg = build_config(spec, &net).await?;
        let conn = PgConnection::open(cfg, spec.tls.clone()).await?;
        Ok(Box::new(conn))
    }
}

// ── the live connection ──────────────────────────────────────────────────────

/// A connected Postgres session. Keeps the Config + TlsSpec so `switch_database`
/// can reconnect with the same encryption (Postgres cannot change database on a
/// live connection).
struct PgConnection {
    client: Client,
    task: JoinHandle<()>,
    config: Config,
    tls: crate::spec::TlsSpec,
    encrypted: bool,
}

impl PgConnection {
    /// Open a connection honouring the TLS posture. `Require`/verify modes MUST
    /// negotiate TLS (a plaintext-only server is rejected); `Prefer` uses TLS when
    /// available and falls back to plaintext (with `encrypted=false` surfaced);
    /// `Disable` is the only unencrypted-by-request path.
    async fn open(config: Config, tls: crate::spec::TlsSpec) -> anyhow::Result<Self> {
        use tokio_postgres::config::SslMode;

        // Explicit opt-out → plaintext.
        if !tls.wanted() {
            return Self::open_plain(config, tls, false).await;
        }

        // Encrypted attempt via rustls.
        let client_cfg = crate::tls::client_config(&tls)?;
        let connector = tokio_postgres_rustls::MakeRustlsConnect::new((*client_cfg).clone());
        let mut enc_cfg = config.clone();
        enc_cfg.ssl_mode(SslMode::Require);
        match enc_cfg.connect(connector).await {
            Ok((client, connection)) => {
                let task = tokio::spawn(async move {
                    let _ = connection.await;
                });
                Ok(Self {
                    client,
                    task,
                    config,
                    tls,
                    encrypted: true,
                })
            }
            Err(e) => {
                if tls.required() {
                    // Do NOT fall back — reject rather than send credentials in the clear.
                    Err(anyhow::anyhow!(
                        "postgres: TLS is required but the server did not accept it: {e}"
                    ))
                } else {
                    // Prefer: fall back to plaintext, surfaced via encrypted=false.
                    Self::open_plain(config, tls, false).await
                }
            }
        }
    }

    /// Open a plaintext connection (Disable, or a Prefer fallback).
    async fn open_plain(config: Config, tls: crate::spec::TlsSpec, encrypted: bool) -> anyhow::Result<Self> {
        let (client, connection) = config
            .connect(NoTls)
            .await
            .map_err(|e| anyhow::anyhow!("postgres connect failed: {e}"))?;
        let task = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Self {
            client,
            task,
            config,
            tls,
            encrypted,
        })
    }

    /// Run a statement via the simple (text) protocol, collecting typed-as-text rows.
    async fn simple(&self, sql: &str) -> anyhow::Result<(Vec<Column>, Vec<Vec<Value>>, Option<u64>)> {
        let msgs = self.client.simple_query(sql).await.map_err(pg_error)?;
        let mut columns: Vec<Column> = Vec::new();
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut affected: Option<u64> = None;
        for msg in msgs {
            match msg {
                SimpleQueryMessage::Row(row) => {
                    if columns.is_empty() {
                        columns = row
                            .columns()
                            .iter()
                            .map(|c| Column {
                                name: c.name().to_string(),
                                type_name: String::new(),
                            })
                            .collect();
                    }
                    let mut out = Vec::with_capacity(row.len());
                    for i in 0..row.len() {
                        out.push(match row.get(i) {
                            Some(s) => Value::Text(s.to_string()),
                            None => Value::Null,
                        });
                    }
                    rows.push(out);
                }
                SimpleQueryMessage::CommandComplete(n) => affected = Some(n),
                _ => {}
            }
        }
        // A statement that returned rows is a query, not an affected-count op.
        if !rows.is_empty() {
            affected = None;
        }
        Ok((columns, rows, affected))
    }
}

#[async_trait]
impl Connection for PgConnection {
    async fn databases(&mut self) -> anyhow::Result<Vec<String>> {
        let (_c, rows, _a) = self
            .simple("SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY datname")
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| match r.into_iter().next() {
                Some(Value::Text(s)) => Some(s),
                _ => None,
            })
            .collect())
    }

    async fn switch_database(&mut self, db: &str) -> anyhow::Result<()> {
        let mut cfg = self.config.clone();
        cfg.dbname(db);
        let fresh = PgConnection::open(cfg, self.tls.clone()).await?;
        self.task.abort();
        self.client = fresh.client;
        self.task = fresh.task;
        self.config = fresh.config;
        self.encrypted = fresh.encrypted;
        Ok(())
    }

    async fn structure(&mut self) -> anyhow::Result<Vec<Node>> {
        let (_c, rows, _a) = self
            .simple(
                "SELECT table_schema, table_name, table_type \
                 FROM information_schema.tables \
                 WHERE table_schema NOT IN ('pg_catalog', 'information_schema', 'crdb_internal') \
                 ORDER BY table_schema, table_name",
            )
            .await?;
        // Group tables/views under their schema node, preserving order.
        let mut schemas: Vec<Node> = Vec::new();
        for r in rows {
            let get = |i: usize| match r.get(i) {
                Some(Value::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let schema = get(0);
            let name = get(1);
            let ttype = get(2);
            let kind = if ttype.eq_ignore_ascii_case("VIEW") {
                "view"
            } else {
                "table"
            };
            let node = Node {
                name,
                kind: kind.to_string(),
                schema: Some(schema.clone()),
                children: Vec::new(),
            };
            match schemas.iter_mut().find(|s| s.name == schema) {
                Some(s) => s.children.push(node),
                None => schemas.push(Node {
                    name: schema.clone(),
                    kind: "schema".to_string(),
                    schema: None,
                    children: vec![node],
                }),
            }
        }
        Ok(schemas)
    }

    /// `pg_stat_activity` and `pg_locks`, as tables.
    ///
    /// Read as they are rather than summarised: this is the view somebody
    /// opens to answer "what is that query doing and whose is it", and a
    /// summary would be the answer to a different question.
    ///
    /// UNVERIFIED against a server, like `metrics` below: written from the
    /// documented shape of the views. Both are reads, and a wrong column name
    /// shows as an error in the panel rather than as wrong numbers.
    async fn inspect(
        &mut self,
        name: &str,
    ) -> anyhow::Result<(Vec<Column>, Vec<Vec<Value>>)> {
        let sql = match name {
            // `pid` FIRST, because that is the key `act` is given — a view
            // whose key is not in it cannot be acted on from the row.
            "activity" => {
                "SELECT pid, usename AS user, application_name AS application,                  client_addr::text AS client, state,                  to_char(now() - query_start, 'HH24:MI:SS') AS running_for,                  wait_event_type AS waiting_on, left(query, 200) AS query                  FROM pg_stat_activity                  WHERE datname = current_database()                  ORDER BY query_start NULLS LAST"
            }
            "locks" => {
                "SELECT l.pid, l.locktype AS kind, l.mode, l.granted,                  coalesce(c.relname, '') AS relation, a.usename AS user,                  left(a.query, 200) AS query                  FROM pg_locks l                  LEFT JOIN pg_class c ON c.oid = l.relation                  LEFT JOIN pg_stat_activity a ON a.pid = l.pid                  ORDER BY l.granted, l.pid"
            }
            other => anyhow::bail!("this driver has no live view called '{other}'"),
        };
        let (columns, rows, _) = self.simple(sql).await?;
        Ok((columns, rows))
    }

    /// Terminate a session, by its backend pid.
    ///
    /// `pg_terminate_backend` rather than `pg_cancel_backend`: the view offers
    /// "terminate", and cancelling a statement while leaving the session open
    /// is a different act with a different name. Doing the gentler thing under
    /// the harsher word would be lying about what was done.
    async fn act(&mut self, name: &str, key: &str) -> anyhow::Result<String> {
        if name != "activity" {
            anyhow::bail!("nothing can be done to a row of '{name}'");
        }
        // The key comes back from a row this engine itself produced, but it is
        // going into a statement, so it is checked rather than trusted: a pid
        // is digits.
        let pid: i64 = key
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("a backend id is a number, not {key:?}"))?;
        let (_, rows, _) = self
            .simple(&format!("SELECT pg_terminate_backend({pid})"))
            .await?;
        let ok = matches!(rows.first().and_then(|r| r.first()), Some(Value::Bool(true)))
            || matches!(rows.first().and_then(|r| r.first()), Some(Value::Text(t)) if t == "t");
        Ok(if ok {
            format!("terminated backend {pid}")
        } else {
            // It answers false when the pid is already gone, which is not a
            // failure and must not be reported as success either.
            format!("backend {pid} was not there to terminate")
        })
    }

    async fn metrics(&mut self) -> anyhow::Result<Vec<crate::spec::Metric>> {
        use crate::spec::Metric;
        // PostgreSQL's own statistics views. Reads, every one.
        //
        // UNVERIFIED: written from the documented shape of these views, not run
        // against a server — there is none to run it against here. The queries
        // are reads and a failure shows as an error in the panel rather than as
        // wrong numbers, which is the one thing that must not happen.
        let mut out = Vec::new();
        let text = |r: &Vec<Value>, i: usize| -> String {
            match r.get(i) {
                Some(Value::Text(s)) => s.clone(),
                Some(Value::Int(n)) => n.to_string(),
                Some(Value::Float(f)) => f.to_string(),
                Some(Value::Bool(b)) => b.to_string(),
                _ => String::new(),
            }
        };

        if let Ok((_, rows, _)) = self
            .simple("SELECT pg_size_pretty(pg_database_size(current_database())), current_database(), version()")
            .await
        {
            if let Some(r) = rows.first() {
                out.push(Metric { group: "storage", name: "database size", value: text(r, 0) });
                out.push(Metric { group: "settings", name: "database", value: text(r, 1) });
                out.push(Metric { group: "settings", name: "version", value: text(r, 2) });
            }
        }
        if let Ok((_, rows, _)) = self
            .simple(
                "SELECT numbackends, xact_commit, xact_rollback, blks_read, blks_hit, \
                 deadlocks, temp_files FROM pg_stat_database WHERE datname = current_database()",
            )
            .await
        {
            if let Some(r) = rows.first() {
                out.push(Metric { group: "activity", name: "connections", value: text(r, 0) });
                out.push(Metric { group: "activity", name: "commits", value: text(r, 1) });
                out.push(Metric { group: "activity", name: "rollbacks", value: text(r, 2) });
                out.push(Metric { group: "activity", name: "blocks read", value: text(r, 3) });
                out.push(Metric { group: "activity", name: "blocks hit", value: text(r, 4) });
                out.push(Metric { group: "activity", name: "deadlocks", value: text(r, 5) });
                out.push(Metric { group: "activity", name: "temp files", value: text(r, 6) });
            }
        }
        if let Ok((_, rows, _)) = self
            .simple("SELECT count(*) FROM pg_stat_activity WHERE state = 'active'")
            .await
        {
            if let Some(r) = rows.first() {
                out.push(Metric { group: "activity", name: "running queries", value: text(r, 0) });
            }
        }
        if out.is_empty() {
            // A server that answers none of this is usually a hosted one that
            // withholds the statistics views. Saying so beats an empty panel.
            anyhow::bail!(
                "this server answered none of the statistics queries \u{2014} a managed service often \
                 withholds pg_stat_*"
            );
        }
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjRef) -> anyhow::Result<Vec<TableColumn>> {
        // Bindable params aren't available on the simple protocol; the identifiers come from our own schema
        // tree (not user free-text), and are quoted. The primary flag joins pg_index — information_schema
        // does not carry it.
        let schema_pred = match &obj.schema {
            Some(s) => format!("AND c.table_schema = '{}'", s.replace('\'', "''")),
            None => String::new(),
        };
        // Build a DOUBLE-QUOTED identifier path so `::regclass` preserves case / special chars (unquoted
        // regclass lowercases, so `"MyTable"` errored the whole statement → grid wrongly read-only), then
        // '-escape the whole for the outer '...' literal.
        let dq = |x: &str| x.replace('"', "\"\"");
        let ident = match &obj.schema {
            Some(s) => format!("\"{}\".\"{}\"", dq(s), dq(&obj.name)),
            None => format!("\"{}\"", dq(&obj.name)),
        };
        let qual = ident.replace('\'', "''");
        let sql = format!(
            "SELECT c.column_name, c.data_type, (pk.col IS NOT NULL) AS is_pk \
             FROM information_schema.columns c \
             LEFT JOIN ( \
               SELECT a.attname AS col FROM pg_index i \
               JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
               WHERE i.indrelid = '{}'::regclass AND i.indisprimary \
             ) pk ON pk.col = c.column_name \
             WHERE c.table_name = '{}' {} ORDER BY c.ordinal_position",
            qual,
            obj.name.replace('\'', "''"),
            schema_pred
        );
        let (_c, rows, _a) = self.simple(&sql).await?;
        Ok(rows
            .into_iter()
            .map(|r| TableColumn {
                name: match r.first() {
                    Some(Value::Text(s)) => s.clone(),
                    _ => String::new(),
                },
                type_name: match r.get(1) {
                    Some(Value::Text(s)) => s.clone(),
                    _ => String::new(),
                },
                primary: match r.get(2) {
                    Some(Value::Bool(b)) => *b,
                    Some(Value::Text(s)) => s == "t" || s == "true",
                    _ => false,
                },
            })
            .collect())
    }
    async fn indexes(&mut self, obj: &ObjRef) -> anyhow::Result<Vec<Index>> {
        // pg_index is the catalog's own truth. `indkey` is the ordered attribute vector, so unnesting it
        // WITH ORDINALITY keeps the index's COLUMN ORDER (which matters — an index on (a,b) is not (b,a));
        // ordering by that ordinal is what makes the grouping below correct. Expression indexes have
        // attnum 0 and no pg_attribute row, so they surface as NULL and are skipped rather than shown blank.
        let schema_pred = match &obj.schema {
            Some(sc) => format!("AND n.nspname = '{}'", sc.replace('\'', "''")),
            None => String::new(),
        };
        let sql = format!(
            "SELECT i.relname, ix.indisunique, ix.indisprimary, a.attname \
             FROM pg_class t \
             JOIN pg_index ix ON t.oid = ix.indrelid \
             JOIN pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             LEFT JOIN unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) ON true \
             LEFT JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
             WHERE t.relname = '{}' {} \
             ORDER BY i.relname, k.ord",
            obj.name.replace('\'', "''"),
            schema_pred
        );
        let (_c, rows, _a) = self.simple(&sql).await?;
        Ok(super::group_indexes(rows))
    }

    async fn execute(&mut self, stmt: &str) -> anyhow::Result<Box<dyn ResultStream>> {
        let (mut columns, rows, affected) = self.simple(stmt).await?;
        // Recover column TYPES via an extended-protocol describe — the simple/text protocol reports names
        // only, so `simple` leaves `type_name` empty. Only the user-query path pays this extra round trip
        // (the catalog helpers call `simple` directly and do not need types). Best-effort: a multi-statement
        // or otherwise non-preparable SQL leaves the types empty and the grid falls back to text — the engine
        // still validates every write. `prepare` describes only; it does NOT execute, so there is no double
        // run of the statement `simple_query` already ran.
        if !columns.is_empty() {
            if let Ok(prepared) = self.client.prepare(stmt).await {
                for (col, pc) in columns.iter_mut().zip(prepared.columns()) {
                    col.type_name = pc.type_().name().to_string();
                }
            }
        }
        Ok(Box::new(super::buffered::BufferedStream::new(columns, rows, affected)))
    }

    fn cancel_token(&self) -> Option<Box<dyn CancelHandle>> {
        Some(Box::new(PgCancel {
            token: self.client.cancel_token(),
        }))
    }

    fn encrypted(&self) -> bool {
        self.encrypted
    }

    async fn close(self: Box<Self>) -> anyhow::Result<()> {
        self.task.abort();
        Ok(())
    }
}

/// Postgres protocol-level cancel: issues a CancelRequest on its own connection.
struct PgCancel {
    token: tokio_postgres::CancelToken,
}

#[async_trait]
impl CancelHandle for PgCancel {
    async fn cancel(&self) -> anyhow::Result<()> {
        self.token
            .cancel_query(NoTls)
            .await
            .map_err(|e| anyhow::anyhow!("cancel failed: {e}"))
    }
}
