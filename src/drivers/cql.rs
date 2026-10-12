// lode/drivers/cql: the Cassandra / ScyllaDB driver (CQL, scylla crate).
//
// One impl serves both the `cassandra` and `scylla` registry kinds — the scylla
// driver is a production CQL client, Cassandra-compatible. A CQL keyspace maps
// to a "schema" and tables to its children. Values come back as CqlValue and are
// rendered text-first. NOTE: implemented and compile-checked against the scylla
// crate, but NOT runtime-verified in this build environment (no Cassandra/Scylla
// node was available).

use async_trait::async_trait;
use base64::Engine as _;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::frame::response::result::{ColumnType, NativeType};
use scylla::value::{CqlValue, Row};

use crate::driver::{Connection, Driver, ResultStream};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, AuthSpec, Caps, Column, ConnSpec, DriverMeta, Index, Node, ObjRef, ParamSpec, ParamType, RowOutcome,
    RowPlan, TableColumn, Value,
};

/// Cassandra's WHERE is NOT SQL's. It filters on key columns and indexed ones,
/// it has no LIKE and no `IS NULL` — a row either has a value or has no cell at
/// all. Offering the shared list here would put four choices in front of
/// somebody that produce an error every time, so this list is the short truth.
const CQL_BUILDER_OPS: &[crate::spec::BuilderOp] = &[
    crate::spec::BuilderOp { label: "=", fragment: "{col} = {value}" },
    crate::spec::BuilderOp { label: "<", fragment: "{col} < {value}" },
    crate::spec::BuilderOp { label: "<=", fragment: "{col} <= {value}" },
    crate::spec::BuilderOp { label: ">", fragment: "{col} > {value}" },
    crate::spec::BuilderOp { label: ">=", fragment: "{col} >= {value}" },
    crate::spec::BuilderOp { label: "IN", fragment: "{col} IN ({value})" },
    crate::spec::BuilderOp { label: "CONTAINS", fragment: "{col} CONTAINS {value}" },
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
        default: Some("9042"),
    },
    ParamSpec {
        key: "keyspace",
        label: "Keyspace",
        kind: ParamType::String,
        required: false,
        secret: false,
        default: None,
    },
];

const AUTH: &[AuthKind] = &[AuthKind::None, AuthKind::Password];

const CAPS: Caps = Caps {
    sql: true,
    schemas: true,
    cancel: false,
    tls: true,
    tunnel: true,
    multi_db: true,
    kv: false,
    indexes: true,
    ddl: false, // system_schema.indexes; DESCRIBE is reconstructed CLIENT-side in CQL
    // A CQL row edit is `IF EXISTS`: the server itself answers whether it
    // applied, which is the guarantee the other drivers get from a
    // transaction Cassandra does not have. See `write_row`.
    edit: true,
    metrics: false,
};

/// Cassandra's own.
///
/// NO `alter_type`: Cassandra removed changing a column's type — the stored
/// bytes are the old type's and nothing rewrites them. Its index statement
/// has no UNIQUE either, because a secondary index there does not enforce
/// one.
const CQL_DDL_OPS: &[crate::spec::DdlOp] = &[
    crate::spec::SQL_DDL_COMMON[0],
    crate::spec::SQL_DDL_COMMON[1],
    crate::spec::SQL_DDL_COMMON[2],
    crate::spec::DdlOp {
        op: "create_index",
        label: "Index this column\u{2026}",
        template: "CREATE INDEX {index} ON {qualified} ({columns});",
        asks: crate::spec::ASK_INDEX,
    },
    crate::spec::SQL_DDL_COMMON[4],
];

const CASSANDRA_META: DriverMeta = DriverMeta {
    kind: "cassandra",
    display: "Cassandra",
    default_port: Some(9042),
    params: PARAMS,
    auth: AUTH,
    caps: CAPS,
    hint: "SELECT * FROM table LIMIT 100",
    browse: "SELECT {columns} FROM {qualified} {clauses} LIMIT {limit};",
    browse_by_kind: &[],
    query_form: &[],
    query_template: "",
    browse_form: crate::spec::SQL_BROWSE_FORM,
    time_filter: crate::spec::SQL_TIME_FILTER,
    builder_ops: CQL_BUILDER_OPS,
    ddl_ops: CQL_DDL_OPS,
    types: crate::spec::CQL_TYPES,
    inspectors: &[],
};

const SCYLLA_META: DriverMeta = DriverMeta {
    kind: "scylla",
    display: "ScyllaDB",
    default_port: Some(9042),
    params: PARAMS,
    auth: AUTH,
    caps: CAPS,
    hint: "SELECT * FROM table LIMIT 100",
    browse: "SELECT {columns} FROM {qualified} {clauses} LIMIT {limit};",
    browse_by_kind: &[],
    query_form: &[],
    query_template: "",
    browse_form: crate::spec::SQL_BROWSE_FORM,
    time_filter: crate::spec::SQL_TIME_FILTER,
    builder_ops: CQL_BUILDER_OPS,
    ddl_ops: CQL_DDL_OPS,
    types: crate::spec::CQL_TYPES,
    inspectors: &[],
};

/// The CQL driver, serving both the `cassandra` and `scylla` kinds.
pub struct CqlDriver {
    meta: &'static DriverMeta,
}

impl CqlDriver {
    #[cfg(feature = "cassandra")]
    pub fn cassandra() -> Self {
        Self { meta: &CASSANDRA_META }
    }

    #[cfg(feature = "scylla")]
    pub fn scylla() -> Self {
        Self { meta: &SCYLLA_META }
    }
}

#[async_trait]
impl Driver for CqlDriver {
    fn meta(&self) -> &'static DriverMeta {
        self.meta
    }

    async fn connect(&self, spec: &ConnSpec, net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        // Native CQL TLS is not yet wired (scylla's rustls TLS context). Rather
        // than silently send credentials in the clear, a REQUIRED-encryption
        // connection is refused — use an SSH tunnel for an encrypted CQL link
        // meanwhile. (Not runtime-verified: no Cassandra/Scylla node available.)
        if spec.tls.required() && !net.tunneled() {
            return Err(anyhow::anyhow!(
                "CQL native TLS is not implemented in this build; use an SSH tunnel for encryption, \
                 or set tls.mode=disable to connect in the clear explicitly"
            ));
        }
        let host = spec.param("host")?;
        let port = spec.port(9042);
        let addr = net.resolve(host, port).await?;

        let mut builder = SessionBuilder::new().known_node(addr);
        if let AuthSpec::Password { user, password } = &spec.auth {
            builder = builder.user(user.clone(), password.resolve().await?);
        }
        if let Some(ks) = spec.param_opt("keyspace") {
            builder = builder.use_keyspace(ks.to_string(), false);
        }
        let session = builder
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("cql connect failed: {e}"))?;
        Ok(Box::new(CqlConnection {
            session,
            keyspace: spec.param_opt("keyspace").map(|s| s.to_string()),
        }))
    }
}

/// Convert a CqlValue to our text-first cell Value.
fn cell(v: &CqlValue) -> Value {
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => Value::Text(s.clone()),
        CqlValue::Boolean(b) => Value::Bool(*b),
        CqlValue::TinyInt(i) => Value::Int(*i as i64),
        CqlValue::SmallInt(i) => Value::Int(*i as i64),
        CqlValue::Int(i) => Value::Int(*i as i64),
        CqlValue::BigInt(i) => Value::Int(*i),
        CqlValue::Float(f) => Value::Float(*f as f64),
        CqlValue::Double(f) => Value::Float(*f),
        CqlValue::Blob(b) => Value::Bytes {
            b64: base64::engine::general_purpose::STANDARD.encode(b),
            len: b.len(),
        },
        // Everything else (uuid/timeuuid, inet, date/time/timestamp/duration, counter, decimal/varint,
        // list/set/map/tuple/UDT/vector) → the crate's OWN `Display`, which renders CQL-literal forms
        // (`'2026-07-18'`, `[1,2]`, `{'k':'v'}`, a uuid's hex). This replaces `format!("{:?}")` (Rust Debug:
        // `Map([(Text("k"),Text("v"))])`, `CqlTimestamp(...)`) — never the value, never a valid literal.
        // Decimal/Varint still show as `blobAsDecimal(0x…)`/`blobAsVarint(0x…)` (the crate's choice — a valid
        // CQL expression, not a plain number), but that is honest and round-trippable, unlike the Debug form.
        other => Value::Text(other.to_string()),
    }
}

/// The statement for a row write, with the condition that makes it one.
///
/// `IF EXISTS` is appended to the shared UPDATE/DELETE: in CQL an UPDATE of a
/// row that is not there CREATES it, so the condition is what makes an edit an
/// edit rather than an insert nobody asked for.
fn row_statement(plan: &RowPlan) -> String {
    let (stmt, _) = super::rowsql::row_statement(plan, super::rowsql::ANSI);
    format!("{stmt} IF EXISTS")
}

/// One value, bound as the type the SERVER said the parameter is.
///
/// CQL has no text parameters: a `uuid` column takes a uuid and an `int` takes
/// four bytes, so the value cannot simply be handed over as the text somebody
/// typed. The types come from the PREPARED statement — the server's own
/// answer about what it is expecting — and anything this cannot build is
/// refused BY NAME, rather than bound as something else and written wrong.
fn bound(v: &Value, ty: &ColumnType) -> anyhow::Result<Option<CqlValue>> {
    let native = match ty {
        ColumnType::Native(n) => n,
        other => anyhow::bail!("this driver cannot bind a {other:?} from the grid \u{2014} edit it with a CQL statement"),
    };
    let text = match super::rowsql::bind_of(v) {
        // In CQL, writing NULL deletes the cell. That is what clearing a cell
        // in the grid means, so it is the right answer rather than a refusal.
        super::rowsql::Bind::Null => return Ok(None),
        super::rowsql::Bind::Text(s) => s,
        super::rowsql::Bind::Bytes(raw) => {
            return match native {
                NativeType::Blob => Ok(Some(CqlValue::Blob(raw))),
                _ => Ok(Some(CqlValue::Text(String::from_utf8_lossy(&raw).into_owned()))),
            }
        }
    };
    let whole = |what: &str| -> anyhow::Result<i64> {
        text.trim()
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("a {what} column takes a whole number, and '{text}' is not one"))
    };
    let real = |what: &str| -> anyhow::Result<f64> {
        text.trim()
            .parse::<f64>()
            .map_err(|_| anyhow::anyhow!("a {what} column takes a number, and '{text}' is not one"))
    };
    let out = match native {
        NativeType::Ascii => CqlValue::Ascii(text),
        NativeType::Text => CqlValue::Text(text),
        NativeType::Boolean => CqlValue::Boolean(matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "true" | "t" | "yes" | "1"
        )),
        NativeType::TinyInt => CqlValue::TinyInt(i8::try_from(whole("tinyint")?)?),
        NativeType::SmallInt => CqlValue::SmallInt(i16::try_from(whole("smallint")?)?),
        NativeType::Int => CqlValue::Int(i32::try_from(whole("int")?)?),
        NativeType::BigInt => CqlValue::BigInt(whole("bigint")?),
        NativeType::Float => CqlValue::Float(real("float")? as f32),
        NativeType::Double => CqlValue::Double(real("double")?),
        NativeType::Blob => CqlValue::Blob(text.into_bytes()),
        NativeType::Uuid => CqlValue::Uuid(
            uuid::Uuid::parse_str(text.trim()).map_err(|_| anyhow::anyhow!("'{text}' is not a uuid"))?,
        ),
        NativeType::Timeuuid => CqlValue::Timeuuid(
            uuid::Uuid::parse_str(text.trim())
                .map_err(|_| anyhow::anyhow!("'{text}' is not a timeuuid"))?
                .into(),
        ),
        // Milliseconds since the epoch, which is what the column IS. A date
        // somebody typed is not accepted here, because this driver would have
        // to guess the time zone to turn it into one.
        NativeType::Timestamp => CqlValue::Timestamp(scylla::value::CqlTimestamp(whole(
            "timestamp (milliseconds since the epoch)",
        )?)),
        other => anyhow::bail!(
            "this driver cannot bind a {other:?} from the grid \u{2014} edit it with a CQL statement, \
             where the literal is yours to write"
        ),
    };
    Ok(Some(out))
}

/// A live CQL session.
struct CqlConnection {
    session: Session,
    keyspace: Option<String>,
}

impl CqlConnection {
    async fn run(&self, cql: &str) -> anyhow::Result<(Vec<Column>, Vec<Vec<Value>>)> {
        let result = self
            .session
            .query_unpaged(cql, &[])
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        // A statement with no result set (DDL/DML) has no rows.
        let rows_result = match result.into_rows_result() {
            Ok(r) => r,
            Err(_) => return Ok((Vec::new(), Vec::new())),
        };
        let columns: Vec<Column> = rows_result
            .column_specs()
            .iter()
            .map(|s| Column {
                name: s.name().to_string(),
                type_name: format!("{:?}", s.typ()),
            })
            .collect();
        let mut out: Vec<Vec<Value>> = Vec::new();
        for row in rows_result.rows::<Row>().map_err(|e| anyhow::anyhow!("{e}"))? {
            let row = row.map_err(|e| anyhow::anyhow!("{e}"))?;
            let cells = row
                .columns
                .iter()
                .map(|c| match c {
                    Some(v) => cell(v),
                    None => Value::Null,
                })
                .collect();
            out.push(cells);
        }
        Ok((columns, out))
    }
}

#[async_trait]
impl Connection for CqlConnection {
    async fn databases(&mut self) -> anyhow::Result<Vec<String>> {
        let (_c, rows) = self.run("SELECT keyspace_name FROM system_schema.keyspaces").await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| match r.into_iter().next() {
                Some(Value::Text(s)) => Some(s),
                _ => None,
            })
            .collect())
    }

    async fn switch_database(&mut self, db: &str) -> anyhow::Result<()> {
        self.session
            .use_keyspace(db, false)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        self.keyspace = Some(db.to_string());
        Ok(())
    }

    async fn structure(&mut self) -> anyhow::Result<Vec<Node>> {
        // Group tables under their keyspace (the user keyspaces).
        let (_c, rows) = self
            .run(
                "SELECT keyspace_name, table_name FROM system_schema.tables \
                 WHERE keyspace_name NOT IN ('system','system_schema','system_auth',\
                 'system_distributed','system_traces') ALLOW FILTERING",
            )
            .await?;
        let mut schemas: Vec<Node> = Vec::new();
        for r in rows {
            let get = |i: usize| match r.get(i) {
                Some(Value::Text(s)) => s.clone(),
                _ => String::new(),
            };
            let ks = get(0);
            let name = get(1);
            let node = Node {
                name,
                kind: "table".to_string(),
                schema: Some(ks.clone()),
                children: Vec::new(),
            };
            match schemas.iter_mut().find(|s| s.name == ks) {
                Some(s) => s.children.push(node),
                None => schemas.push(Node {
                    name: ks.clone(),
                    kind: "schema".to_string(),
                    schema: None,
                    children: vec![node],
                }),
            }
        }
        Ok(schemas)
    }

    async fn columns(&mut self, obj: &ObjRef) -> anyhow::Result<Vec<TableColumn>> {
        // In CQL a row is identified by its PARTITION KEY plus its CLUSTERING columns — together the
        // PRIMARY KEY. `kind` names each one, so both count; a plain `regular` column does not.
        let ks = obj.schema.clone().or_else(|| self.keyspace.clone()).unwrap_or_default();
        let sql = format!(
            "SELECT column_name, type, kind FROM system_schema.columns \
             WHERE keyspace_name = '{}' AND table_name = '{}'",
            ks.replace('\'', "''"),
            obj.name.replace('\'', "''")
        );
        let (_c, rows) = self.run(&sql).await?;
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
                primary: matches!(r.get(2), Some(Value::Text(k)) if k == "partition_key" || k == "clustering"),
                references: None,
            })
            .collect())
    }
    async fn indexes(&mut self, obj: &ObjRef) -> anyhow::Result<Vec<Index>> {
        // system_schema.indexes is the cluster's own catalog. A CQL secondary index has exactly ONE target
        // (options['target']) and is never unique nor primary — the partition key is not an index — so those
        // flags are honestly false rather than invented.
        let ks = match &obj.schema {
            Some(sc) => sc.replace('\'', "''"),
            None => return Ok(Vec::new()),
        };
        let sql = format!(
            "SELECT index_name, options FROM system_schema.indexes \
             WHERE keyspace_name = '{}' AND table_name = '{}'",
            ks,
            obj.name.replace('\'', "''")
        );
        let (_c, rows) = self.run(&sql).await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let name = match r.first() {
                    Some(Value::Text(s)) => s.clone(),
                    _ => return None,
                };
                // `options` is a CQL map<text,text>, now rendered by the crate's `Display` as a CQL map
                // literal: `{'target':'email','class_name':'exams'}` (keys/values single-quoted). Find the
                // `target` key and take its quoted value. (This tracks the cell() render above — when that was
                // Rust Debug this sliced `Text("target")` instead; both were updated together.)
                let target = match r.get(1) {
                    Some(Value::Text(s)) => s
                        .split_once("'target':'")
                        .and_then(|(_, rest)| rest.split_once('\'').map(|(v, _)| v.to_string())),
                    _ => None,
                };
                Some(Index {
                    name,
                    columns: target.into_iter().filter(|t| !t.is_empty()).collect(),
                    unique: false,
                    primary: false,
                })
            })
            .collect())
    }

    async fn plan_row(&mut self, plan: &RowPlan) -> anyhow::Result<String> {
        Ok(row_statement(plan))
    }

    /// Change or remove one row.
    ///
    /// `IF EXISTS` is the whole of it. Cassandra has no transaction to roll a
    /// write back with, and its UPDATE is an UPSERT: without that condition,
    /// editing a row that somebody else has just deleted would silently
    /// CREATE it again, which is the one thing a row edit must not do. With
    /// it, the server performs the write as a lightweight transaction and
    /// answers whether it applied — the same promise, in Cassandra's own
    /// terms, and the reason this driver can claim the capability at all.
    ///
    /// The key is the full PRIMARY KEY (the engine checks that before this is
    /// reached), so the write addresses exactly one row by definition.
    async fn write_row(&mut self, plan: &RowPlan) -> anyhow::Result<RowOutcome> {
        let statement = row_statement(plan);
        let (_, binds) = super::rowsql::row_statement(plan, super::rowsql::ANSI);
        let prepared = self
            .session
            .prepare(statement.as_str())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let wanted: Vec<ColumnType<'static>> = prepared
            .get_variable_col_specs()
            .iter()
            .map(|c| c.typ().clone().into_owned())
            .collect();
        if wanted.len() != binds.len() {
            anyhow::bail!(
                "the server expects {} value(s) for that statement and this driver has {}",
                wanted.len(),
                binds.len()
            );
        }
        let mut values: Vec<Option<CqlValue>> = Vec::with_capacity(binds.len());
        for (v, ty) in binds.iter().zip(wanted.iter()) {
            values.push(bound(v, ty)?);
        }
        let result = self
            .session
            .execute_unpaged(&prepared, values)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        // A lightweight transaction answers with one row whose first column is
        // `[applied]`. False means the row was not there — somebody else had
        // removed it — and nothing was written.
        let applied = match result.into_rows_result() {
            Ok(rows) => rows
                .rows::<Row>()
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .next()
                .transpose()
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .and_then(|r| r.columns.into_iter().next().flatten())
                .map(|c| !matches!(c, CqlValue::Boolean(false)))
                .unwrap_or(true),
            // No result set at all: the server said nothing to read, which for
            // a conditional write it does not do. Take it as applied.
            Err(_) => true,
        };
        if !applied {
            anyhow::bail!("that row was not there any more \u{2014} nothing was written");
        }
        Ok(RowOutcome { statement, affected: 1 })
    }

    async fn execute(&mut self, stmt: &str) -> anyhow::Result<Box<dyn ResultStream>> {
        let (columns, rows) = self.run(stmt).await?;
        Ok(Box::new(super::buffered::BufferedStream::new(columns, rows, None)))
    }

    async fn close(self: Box<Self>) -> anyhow::Result<()> {
        Ok(())
    }
}
