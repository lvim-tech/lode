// lode/drivers/sqlite: the SQLite driver.
//
// SQLite is an embedded, SYNCHRONOUS engine (a C library), so every operation
// runs on `spawn_blocking` — off the async runtime's worker threads — while the
// same async trait the network drivers use is preserved. The connection is a
// file path (":memory:" for an in-memory database); there is no host/port/auth.
// Values come back through rusqlite's owned Value and render as text.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection as SqliteConn;

use crate::driver::{Connection, Driver, ResultStream};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, Caps, Column, ConnSpec, DriverMeta, Index, Node, ObjRef, ParamSpec, ParamType,
    RowOutcome, RowPlan, TableColumn, Value,
};

const PARAMS: &[ParamSpec] = &[ParamSpec {
    key: "file",
    label: "Database file",
    kind: ParamType::File,
    required: true,
    secret: false,
    default: Some(":memory:"),
}];

const META: DriverMeta = DriverMeta {
    kind: "sqlite",
    display: "SQLite",
    default_port: None,
    params: PARAMS,
    auth: &[AuthKind::None],
    caps: Caps {
        sql: true,
        schemas: true,
        cancel: false, // embedded + fast; no async cancel channel
        tls: false,
        tunnel: false,
        multi_db: false,
        kv: false,
        indexes: true, // PRAGMA index_list / index_info
        ddl: true,     // sqlite_master.sql — the ORIGINAL CREATE text, verbatim
        // sqlite binds every value and rolls back a write that was not one row.
        edit: true,
        // PRAGMAs, all of them reads.
        metrics: true,
    },
    hint: "SELECT * FROM table LIMIT 100",
    browse: "SELECT {columns} FROM {qualified} {clauses} LIMIT {limit};",
    browse_by_kind: &[],
    query_form: &[],
    query_template: "",
    browse_form: crate::spec::SQL_BROWSE_FORM,
    time_filter: crate::spec::SQL_TIME_FILTER,
    builder_ops: crate::spec::SQL_BUILDER_OPS,
    inspectors: &[],
};

/// The SQLite driver.
pub struct SqliteDriver;

impl SqliteDriver {
    pub fn new() -> Self {
        SqliteDriver
    }
}

#[async_trait]
impl Driver for SqliteDriver {
    fn meta(&self) -> &'static DriverMeta {
        &META
    }

    async fn connect(&self, spec: &ConnSpec, _net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        let file = spec.param_path("file")?;
        let conn = tokio::task::spawn_blocking(move || {
            if file.as_str() == ":memory:" {
                SqliteConn::open_in_memory()
            } else {
                SqliteConn::open(&file)
            }
        })
        .await?
        .map_err(|e| anyhow::anyhow!("sqlite open failed: {e}"))?;
        Ok(Box::new(SqliteConnection {
            conn: Arc::new(Mutex::new(conn)),
        }))
    }
}

/// Convert an owned rusqlite Value to our text-first cell Value.
fn cell(v: SqlValue) -> Value {
    match v {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) => Value::Int(i),
        SqlValue::Real(f) => Value::Float(f),
        SqlValue::Text(s) => Value::Text(s),
        SqlValue::Blob(b) => Value::Bytes {
            b64: base64::engine::general_purpose::STANDARD.encode(&b),
            len: b.len(),
        },
    }
}

/// Bytes as a person reads them. A page count times a page size is a number
/// nobody can hold; "4.0 MiB" is the same fact and can be acted on.
fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn display(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// Build the statement for a row write, and the values to bind to it.
///
/// Every value is a `?` — a name can be an identifier here because it came from
/// the engine's own `columns()`, but a VALUE never becomes syntax. The order of
/// the binds is the order of the placeholders: the SET values first, then the
/// key's, because that is the order they appear in.
fn row_statement(plan: &RowPlan) -> (String, Vec<SqlValue>) {
    let table = qualify(&plan.obj);
    let mut binds: Vec<SqlValue> = Vec::new();
    let stmt = if plan.delete {
        format!("DELETE FROM {table} WHERE {}", where_of(&plan.key, &mut binds))
    } else {
        let sets: Vec<String> = plan
            .set
            .iter()
            .map(|(c, v)| {
                binds.push(bind(&crate::spec::value_from_json(v)));
                format!("{} = ?", ident(c))
            })
            .collect();
        format!(
            "UPDATE {table} SET {} WHERE {}",
            sets.join(", "),
            where_of(&plan.key, &mut binds)
        )
    };
    (stmt, binds)
}

/// The WHERE for an identifying key. A NULL in a key is compared with `IS NULL`,
/// since `= NULL` matches nothing and would silently write no rows — which this
/// driver would then refuse as "matched 0 rows", correctly but confusingly.
fn where_of(key: &[(String, serde_json::Value)], binds: &mut Vec<SqlValue>) -> String {
    key.iter()
        .map(|(c, v)| {
            let v = crate::spec::value_from_json(v);
            if matches!(v, Value::Null) {
                format!("{} IS NULL", ident(c))
            } else {
                binds.push(bind(&v));
                format!("{} = ?", ident(c))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn qualify(obj: &ObjRef) -> String {
    match obj.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", ident(s), ident(&obj.name)),
        None => ident(&obj.name),
    }
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn bind(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Int(i) => SqlValue::Integer(*i),
        Value::Float(f) => SqlValue::Real(*f),
        Value::Text(s) => SqlValue::Text(s.clone()),
        Value::Bytes { b64, .. } => match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(bytes) => SqlValue::Blob(bytes),
            // Undecodable base64 is a bug upstream, not a reason to write
            // something else into the column.
            Err(_) => SqlValue::Null,
        },
        // Everything left is a shape SQLite has no column type for. It goes in
        // as the text it displays as, which is what SQLite would store anyway.
        Value::Json(j) => SqlValue::Text(j.to_string()),
        Value::Timestamp(s) | Value::Oid(s) => SqlValue::Text(s.clone()),
        Value::Ext { text, .. } => SqlValue::Text(text.clone()),
    }
}

/// A live SQLite connection. The engine is behind a mutex so blocking calls run
/// on spawn_blocking with a 'static handle.
struct SqliteConnection {
    conn: Arc<Mutex<SqliteConn>>,
}

impl SqliteConnection {
    /// Run one statement on a blocking thread. A statement that returns no columns
    /// (DDL/DML) reports its changed-row count as `affected`.
    async fn run(&self, sql: String) -> anyhow::Result<(Vec<Column>, Vec<Vec<Value>>, Option<u64>)> {
        let arc = self.conn.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let conn = arc.lock().unwrap();
            let stmt = conn.prepare(&sql).map_err(|e| anyhow::anyhow!("{e}"))?;
            let ncol = stmt.column_count();
            if ncol == 0 {
                // Run the ENTIRE batch — `prepare` parses only the FIRST statement, so a multi-statement
                // migration would otherwise execute only its first `;` silently. execute_batch runs them all.
                drop(stmt);
                conn.execute_batch(&sql).map_err(|e| anyhow::anyhow!("{e}"))?;
                return Ok((Vec::new(), Vec::new(), Some(conn.changes())));
            }
            let mut stmt = stmt;
            // The DECLARED type, where SQLite has one.
            //
            // It said nothing at all before, which was defensible while the
            // type was only ever shown — SQLite's types are advisory, and a
            // column declared INTEGER can hold a string. It is not defensible
            // now that the engine derives a `kind` from it: saying nothing
            // means every column of every SQLite query reads as `unknown`,
            // and a client that could have put a declared INTEGER on a value
            // axis has to go back to guessing from the page.
            //
            // `decl_type` is None for an expression (`COUNT(*)`, `a + b`),
            // which is correct: that column has no declared type, and an
            // empty string is how this struct says so.
            let columns: Vec<Column> = stmt
                .columns()
                .iter()
                .map(|c| Column {
                    name: c.name().to_string(),
                    type_name: c.decl_type().unwrap_or_default().to_string(),
                })
                .collect();
            let mut out: Vec<Vec<Value>> = Vec::new();
            let mut rows = stmt.query([]).map_err(|e| anyhow::anyhow!("{e}"))?;
            while let Some(r) = rows.next().map_err(|e| anyhow::anyhow!("{e}"))? {
                let mut row = Vec::with_capacity(ncol);
                for i in 0..ncol {
                    let v: SqlValue = r.get(i).map_err(|e| anyhow::anyhow!("{e}"))?;
                    row.push(cell(v));
                }
                out.push(row);
            }
            Ok((columns, out, None))
        })
        .await?
    }
}

#[async_trait]
impl Connection for SqliteConnection {
    async fn databases(&mut self) -> anyhow::Result<Vec<String>> {
        let (_c, rows, _a) = self.run("PRAGMA database_list".to_string()).await?;
        // PRAGMA database_list → (seq, name, file); the name is column 1.
        Ok(rows
            .into_iter()
            .filter_map(|r| match r.get(1) {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect())
    }

    async fn switch_database(&mut self, _db: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("SQLite has a single database per connection"))
    }

    async fn structure(&mut self) -> anyhow::Result<Vec<Node>> {
        let (_c, rows, _a) = self
            .run(
                "SELECT name, type FROM sqlite_master WHERE type IN ('table','view') \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name"
                    .to_string(),
            )
            .await?;
        let children = rows
            .into_iter()
            .map(|r| {
                let name = match r.first() {
                    Some(Value::Text(s)) => s.clone(),
                    _ => String::new(),
                };
                let kind = match r.get(1) {
                    Some(Value::Text(s)) if s == "view" => "view",
                    _ => "table",
                };
                Node {
                    name,
                    kind: kind.to_string(),
                    schema: Some("main".to_string()),
                    children: Vec::new(),
                }
            })
            .collect();
        Ok(vec![Node {
            name: "main".to_string(),
            kind: "schema".to_string(),
            schema: None,
            children,
        }])
    }

    async fn columns(&mut self, obj: &ObjRef) -> anyhow::Result<Vec<TableColumn>> {
        let esc = obj.name.replace('\'', "''");
        // WHAT POINTS WHERE, first. `foreign_key_list` → (id, seq, table,
        // from, to, on_update, on_delete, match); `from` is the column here
        // and `to` the column there — and `to` is NULL when the reference is
        // written without one, which means "the other table's primary key".
        // Left as None in that case rather than filled in with a guess: the
        // key would have to be read from the other table, and a reference
        // naming the wrong column is worse than one naming none.
        let mut refs: std::collections::HashMap<String, crate::spec::Ref> = Default::default();
        if let Ok((_c, fks, _a)) = self.run(format!("PRAGMA foreign_key_list('{esc}')")).await {
            for r in fks {
                let text = |i: usize| match r.get(i) {
                    Some(Value::Text(s)) => Some(s.clone()),
                    _ => None,
                };
                if let (Some(table), Some(from), Some(to)) = (text(2), text(3), text(4)) {
                    refs.insert(from, crate::spec::Ref { table, schema: None, column: to });
                }
            }
        }
        let sql = format!("PRAGMA table_info('{esc}')");
        let (_c, rows, _a) = self.run(sql).await?;
        // table_info → (cid, name, type, notnull, dflt, pk); name=1, type=2, pk=5.
        // `pk` is the 1-based position within the primary key (0 = not in it), which is ALSO how an
        // `INTEGER PRIMARY KEY` reports — and that column has no index at all (it is the rowid alias), so
        // this PRAGMA is the only place sqlite admits to having a key.
        Ok(rows
            .into_iter()
            .map(|r| TableColumn {
                name: match r.get(1) {
                    Some(Value::Text(s)) => s.clone(),
                    _ => String::new(),
                },
                type_name: match r.get(2) {
                    Some(Value::Text(s)) => s.clone(),
                    _ => String::new(),
                },
                primary: matches!(r.get(5), Some(Value::Int(i)) if *i > 0),
                references: match r.get(1) {
                    Some(Value::Text(s)) => refs.get(s).cloned(),
                    _ => None,
                },
            })
            .collect())
    }
    async fn indexes(&mut self, obj: &ObjRef) -> anyhow::Result<Vec<Index>> {
        let esc = obj.name.replace('\'', "''");
        // index_list → (seq, name, unique, origin, partial). `origin` is 'pk' for the implicit primary-key
        // index, 'u' for a UNIQUE constraint, 'c' for a plain CREATE INDEX.
        let (_c, rows, _a) = self.run(format!("PRAGMA index_list('{esc}')")).await?;
        let mut out = Vec::new();
        for r in rows {
            let name = match r.get(1) {
                Some(Value::Text(s)) => s.clone(),
                _ => continue,
            };
            let unique = matches!(r.get(2), Some(Value::Int(1)));
            let primary = matches!(r.get(3), Some(Value::Text(s)) if s == "pk");
            // index_info → (seqno, cid, name); a name of NULL marks an expression index's computed column,
            // which has no column to name — skip it rather than showing an empty row.
            let iesc = name.replace('\'', "''");
            let (_c2, irows, _a2) = self.run(format!("PRAGMA index_info('{iesc}')")).await?;
            let columns = irows
                .into_iter()
                .filter_map(|ir| match ir.get(2) {
                    Some(Value::Text(s)) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            out.push(Index {
                name,
                columns,
                unique,
                primary,
            });
        }
        Ok(out)
    }

    async fn ddl(&mut self, obj: &ObjRef) -> anyhow::Result<Option<String>> {
        // sqlite stores the ORIGINAL text of each CREATE, so this is the engine's own answer verbatim.
        // `sql` is NULL for the implicit indexes sqlite creates itself — not reachable here (we only ask for
        // tables/views), but the Text match below leaves any such row as None rather than an empty string.
        let esc = obj.name.replace('\'', "''");
        let (_c, rows, _a) = self
            .run(format!(
                "SELECT sql FROM sqlite_master WHERE name = '{esc}' AND type IN ('table','view')"
            ))
            .await?;
        Ok(rows.first().and_then(|r| match r.first() {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        }))
    }

    async fn metrics(&mut self) -> anyhow::Result<Vec<crate::spec::Metric>> {
        use crate::spec::Metric;
        // SQLite's system views are PRAGMAs. Every one of these is a read.
        let mut out = Vec::new();
        let one = |rows: Vec<Vec<Value>>| -> Option<Value> { rows.into_iter().next()?.into_iter().next() };
        let num = |v: Option<Value>| -> Option<i64> {
            match v {
                Some(Value::Int(i)) => Some(i),
                Some(Value::Float(f)) => Some(f as i64),
                Some(Value::Text(s)) => s.parse().ok(),
                _ => None,
            }
        };

        let (_, r, _) = self.run("PRAGMA page_count".into()).await?;
        let pages = num(one(r));
        let (_, r, _) = self.run("PRAGMA page_size".into()).await?;
        let page_size = num(one(r));
        if let (Some(p), Some(s)) = (pages, page_size) {
            out.push(Metric { group: "storage", name: "size", value: human_bytes(p * s) });
            out.push(Metric { group: "storage", name: "pages", value: p.to_string() });
            out.push(Metric { group: "storage", name: "page size", value: human_bytes(s) });
        }
        let (_, r, _) = self.run("PRAGMA freelist_count".into()).await?;
        if let Some(f) = num(one(r)) {
            // Free pages are space the file is holding and not using — the
            // number that says whether a VACUUM would give anything back.
            out.push(Metric { group: "storage", name: "free pages", value: f.to_string() });
        }
        for (pragma, name, group) in [
            ("PRAGMA journal_mode", "journal mode", "settings"),
            ("PRAGMA synchronous", "synchronous", "settings"),
            ("PRAGMA encoding", "encoding", "settings"),
            ("PRAGMA foreign_keys", "foreign keys", "settings"),
            ("PRAGMA user_version", "user version", "settings"),
        ] {
            let (_, r, _) = self.run(pragma.into()).await?;
            if let Some(v) = one(r) {
                out.push(Metric { group, name, value: display(&v) });
            }
        }
        let (_, r, _) = self
            .run("SELECT count(*) FROM sqlite_master WHERE type='table'".into())
            .await?;
        if let Some(n) = num(one(r)) {
            out.push(Metric { group: "contents", name: "tables", value: n.to_string() });
        }
        let (_, r, _) = self
            .run("SELECT count(*) FROM sqlite_master WHERE type='index'".into())
            .await?;
        if let Some(n) = num(one(r)) {
            out.push(Metric { group: "contents", name: "indexes", value: n.to_string() });
        }
        Ok(out)
    }

    async fn plan_row(&mut self, plan: &RowPlan) -> anyhow::Result<String> {
        Ok(row_statement(plan).0)
    }

    async fn write_row(&mut self, plan: &RowPlan) -> anyhow::Result<RowOutcome> {
        let (statement, binds) = row_statement(plan);
        let arc = self.conn.clone();
        let sql = statement.clone();
        let affected = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
            let mut conn = arc.lock().unwrap();
            // A TRANSACTION, not because two statements run, but because one
            // might turn out to be wrong about how many rows it hits. rusqlite
            // rolls this back on drop unless it is committed.
            let tx = conn.transaction().map_err(|e| anyhow::anyhow!("{e}"))?;
            let n = tx
                .execute(&sql, rusqlite::params_from_iter(binds.iter()))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if n != 1 {
                // Dropping `tx` rolls it back. Say what happened and leave the
                // table as it was: a write that hit three rows is not a write
                // that succeeded with a surprising number.
                anyhow::bail!(
                    "that key matched {n} row(s), not one \u{2014} nothing was written"
                );
            }
            tx.commit().map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(n as u64)
        })
        .await??;
        Ok(RowOutcome { statement, affected })
    }

    async fn execute(&mut self, stmt: &str) -> anyhow::Result<Box<dyn ResultStream>> {
        let (columns, rows, affected) = self.run(stmt.to_string()).await?;
        Ok(Box::new(super::buffered::BufferedStream::new(columns, rows, affected)))
    }

    async fn close(self: Box<Self>) -> anyhow::Result<()> {
        Ok(())
    }
}
