// lode/store: the engine's OWN persistence — saved connections, saved queries
// and the statement history.
//
// This used to live in the client (lvim-db kept it in a Lua-side SQLite file),
// and that was the one thing stopping two clients from seeing the same state:
// the editor plugin and the GUI would each have had their own list of
// connections. So it moved HERE, into the process that already owns the
// drivers. Both clients now read and write the same three tables through
// `store.*` RPC.
//
// WHAT IS STORED IS THE TEMPLATE, NEVER A CREDENTIAL. A connection's `spec`
// column is the `{ params, auth, tls, tunnel }` JSON exactly as the client's
// form produced it, with secret fields still holding their templates
// (`{{ env … }}`, `{{ vault … }}`). Resolution happens in `secret.rs` at connect
// time and the resolved value never comes back out — not to a response, not to
// a log line, and not to this file.
//
// The schema is deliberately IDENTICAL, column for column, to the Lua schema it
// replaces (see `import`): that turns the migration into a copy rather than a
// transformation, so there is no shape to get wrong and nothing to lose.
//
// SQLite is synchronous, so every operation runs on `spawn_blocking` — the same
// discipline the sqlite/duckdb drivers follow.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value as Json};

/// The schema version stamped into `user_version`. It matches the version the
/// Lua store reached, so a database written by either side reads the same.
const SCHEMA_VERSION: i64 = 2;

/// The history rows `store.history.list` returns when a client names no limit.
const DEFAULT_HISTORY_LIMIT: i64 = 200;

/// Where the database lives, in probe order:
///   `$LODE_DATA_DIR`, `$XDG_DATA_HOME/lode`, `$HOME/.local/share/lode`.
/// The directory is created on first open.
fn data_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("LODE_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Ok(xdg.join("lode"));
        }
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("neither $LODE_DATA_DIR nor $HOME is set"))?;
    Ok(PathBuf::from(home).join(".local/share/lode"))
}

/// The saved-state store. Cheap to clone; the SQLite handle is shared.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl Store {
    /// Open (creating it if need be) the engine's database and apply the schema.
    pub fn open() -> anyhow::Result<Self> {
        let dir = data_dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!("cannot create '{}': {e}", dir.display()))?;
        let path = dir.join("lode.db");
        let conn = Connection::open(&path).map_err(|e| anyhow::anyhow!("cannot open '{}': {e}", path.display()))?;
        // WAL so a second client reading while this one writes does not block,
        // and NORMAL sync because a lost history row after a power cut is not
        // worth an fsync per statement.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        Self::apply_schema(&conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create the three tables if they are absent and stamp the version. Every
    /// statement is `IF NOT EXISTS`, so this is safe on a database an older
    /// client already wrote.
    fn apply_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS connections (
                id      INTEGER PRIMARY KEY AUTOINCREMENT,
                name    TEXT NOT NULL UNIQUE,
                driver  TEXT NOT NULL,
                spec    TEXT NOT NULL,
                created INTEGER
            );
            CREATE TABLE IF NOT EXISTS queries (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                conn       TEXT NOT NULL,
                name       TEXT NOT NULL,
                sql        TEXT NOT NULL,
                updated_at INTEGER
            );
            CREATE UNIQUE INDEX IF NOT EXISTS queries_conn_name ON queries (conn, name);
            CREATE TABLE IF NOT EXISTS history (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                conn      TEXT,
                driver    TEXT,
                statement TEXT NOT NULL,
                state     TEXT,
                ms        INTEGER,
                "rows"    INTEGER,
                ts        INTEGER
            );
            CREATE INDEX IF NOT EXISTS history_ts ON history (ts);
            "#,
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Run `f` against the handle on a blocking thread.
    async fn with<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| anyhow::anyhow!("store lock poisoned"))?;
            f(&guard)
        })
        .await?
    }

    // ── connections ──────────────────────────────────────────────────────────

    /// Every saved connection, newest first. `spec` comes back PARSED, so a
    /// client never has to decode a string inside a decoded object.
    pub async fn list_connections(&self) -> anyhow::Result<Vec<Json>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, driver, spec, created FROM connections \
                 ORDER BY COALESCE(created, 0) DESC, id DESC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(conn_json(
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// One saved connection by name, or `None`.
    pub async fn get_connection(&self, name: String) -> anyhow::Result<Option<Json>> {
        self.with(move |c| {
            let row = c
                .query_row(
                    "SELECT id, name, driver, spec, created FROM connections WHERE name = ?1",
                    params![name],
                    |r| {
                        Ok(conn_json(
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
    }

    /// Insert or update a saved connection, keyed by `name`. An update keeps the
    /// original `created` stamp — the list is ordered by it, and re-saving a
    /// connection is not the same event as creating one.
    pub async fn save_connection(&self, name: String, driver: String, spec: Json) -> anyhow::Result<()> {
        let spec = serde_json::to_string(&spec)?;
        self.with(move |c| {
            c.execute(
                "INSERT INTO connections (name, driver, spec, created) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(name) DO UPDATE SET driver = excluded.driver, spec = excluded.spec",
                params![name, driver, spec, now()],
            )?;
            Ok(())
        })
        .await
    }

    /// Delete a saved connection. Also drops the queries saved under it —
    /// they are scoped BY connection name and would otherwise be unreachable
    /// rows that reappear if a new connection reused the name.
    pub async fn remove_connection(&self, name: String) -> anyhow::Result<bool> {
        self.with(move |c| {
            let n = c.execute("DELETE FROM connections WHERE name = ?1", params![name])?;
            c.execute("DELETE FROM queries WHERE conn = ?1", params![name])?;
            Ok(n > 0)
        })
        .await
    }

    // ── saved queries (scoped per connection) ────────────────────────────────

    pub async fn list_queries(&self, conn_name: String) -> anyhow::Result<Vec<Json>> {
        self.with(move |c| {
            let mut stmt =
                c.prepare("SELECT id, conn, name, sql, updated_at FROM queries WHERE conn = ?1 ORDER BY name ASC")?;
            let rows = stmt
                .query_map(params![conn_name], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "conn": r.get::<_, String>(1)?,
                        "name": r.get::<_, String>(2)?,
                        "sql": r.get::<_, String>(3)?,
                        "updated_at": r.get::<_, Option<i64>>(4)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn get_query(&self, conn_name: String, name: String) -> anyhow::Result<Option<Json>> {
        self.with(move |c| {
            let row = c
                .query_row(
                    "SELECT id, conn, name, sql, updated_at FROM queries WHERE conn = ?1 AND name = ?2",
                    params![conn_name, name],
                    |r| {
                        Ok(json!({
                            "id": r.get::<_, i64>(0)?,
                            "conn": r.get::<_, String>(1)?,
                            "name": r.get::<_, String>(2)?,
                            "sql": r.get::<_, String>(3)?,
                            "updated_at": r.get::<_, Option<i64>>(4)?,
                        }))
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
    }

    pub async fn save_query(&self, conn_name: String, name: String, sql: String) -> anyhow::Result<()> {
        self.with(move |c| {
            c.execute(
                "INSERT INTO queries (conn, name, sql, updated_at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(conn, name) DO UPDATE SET sql = excluded.sql, updated_at = excluded.updated_at",
                params![conn_name, name, sql, now()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn remove_query(&self, conn_name: String, name: String) -> anyhow::Result<bool> {
        self.with(move |c| {
            let n = c.execute(
                "DELETE FROM queries WHERE conn = ?1 AND name = ?2",
                params![conn_name, name],
            )?;
            Ok(n > 0)
        })
        .await
    }

    // ── history ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub async fn record_history(
        &self,
        conn_name: Option<String>,
        driver: Option<String>,
        statement: String,
        state: Option<String>,
        ms: Option<i64>,
        rows: Option<i64>,
    ) -> anyhow::Result<()> {
        self.with(move |c| {
            c.execute(
                "INSERT INTO history (conn, driver, statement, state, ms, \"rows\", ts) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![conn_name, driver, statement, state, ms, rows, now()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn list_history(&self, limit: Option<i64>) -> anyhow::Result<Vec<Json>> {
        let limit = limit.filter(|n| *n > 0).unwrap_or(DEFAULT_HISTORY_LIMIT);
        self.with(move |c| {
            let mut stmt = c.prepare(
                "SELECT id, conn, driver, statement, state, ms, \"rows\", ts FROM history \
                 ORDER BY COALESCE(ts, 0) DESC, id DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "conn": r.get::<_, Option<String>>(1)?,
                        "driver": r.get::<_, Option<String>>(2)?,
                        "statement": r.get::<_, String>(3)?,
                        "state": r.get::<_, Option<String>>(4)?,
                        "ms": r.get::<_, Option<i64>>(5)?,
                        "rows": r.get::<_, Option<i64>>(6)?,
                        "ts": r.get::<_, Option<i64>>(7)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    // ── migration ────────────────────────────────────────────────────────────

    /// Copy what is MISSING out of an older client's own SQLite file.
    ///
    /// The source is opened READ-ONLY and never written, so a failed import
    /// leaves the user's old data exactly as it was and the old client keeps
    /// working. Every insert is `OR IGNORE` against the natural key — `name` for
    /// a connection, `(conn, name)` for a query, `(ts, statement)` for a history
    /// row — which makes the whole thing IDEMPOTENT: running it a second time
    /// imports nothing, so a client may call it on every start without
    /// remembering whether it already did.
    ///
    /// A table that is not in the source is skipped, not an error: the Lua store
    /// grew `queries` in its v2, so a v1 file legitimately has only two tables.
    pub async fn import_from(&self, path: PathBuf) -> anyhow::Result<Json> {
        if !path.exists() {
            return Err(anyhow::anyhow!("no such file: {}", path.display()));
        }
        self.with(move |c| {
            let src = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| anyhow::anyhow!("cannot read '{}': {e}", path.display()))?;

            let mut conns = 0usize;
            if has_table(&src, "connections")? {
                let mut stmt = src.prepare("SELECT name, driver, spec, created FROM connections")?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                for (name, driver, spec, created) in rows {
                    conns += c.execute(
                        "INSERT OR IGNORE INTO connections (name, driver, spec, created) VALUES (?1, ?2, ?3, ?4)",
                        params![name, driver, spec, created.or_else(|| Some(now()))],
                    )?;
                }
            }

            let mut queries = 0usize;
            if has_table(&src, "queries")? {
                let mut stmt = src.prepare("SELECT conn, name, sql, updated_at FROM queries")?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                for (conn_name, name, sql, updated) in rows {
                    queries += c.execute(
                        "INSERT OR IGNORE INTO queries (conn, name, sql, updated_at) VALUES (?1, ?2, ?3, ?4)",
                        params![conn_name, name, sql, updated.or_else(|| Some(now()))],
                    )?;
                }
            }

            let mut history = 0usize;
            if has_table(&src, "history")? {
                let mut stmt = src.prepare("SELECT conn, driver, statement, state, ms, \"rows\", ts FROM history")?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                            r.get::<_, Option<i64>>(5)?,
                            r.get::<_, Option<i64>>(6)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                for (conn_name, driver, statement, state, ms, nrows, ts) in rows {
                    // History has no natural unique key in either schema, so the
                    // duplicate check is an explicit lookup on (ts, statement)
                    // rather than a constraint: adding a UNIQUE index here would
                    // silently drop two genuinely distinct runs that landed in
                    // the same second with the same text.
                    let exists: bool = c.query_row(
                        "SELECT EXISTS(SELECT 1 FROM history WHERE ts IS ?1 AND statement = ?2)",
                        params![ts, statement],
                        |r| r.get(0),
                    )?;
                    if exists {
                        continue;
                    }
                    history += c.execute(
                        "INSERT INTO history (conn, driver, statement, state, ms, \"rows\", ts) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![conn_name, driver, statement, state, ms, nrows, ts],
                    )?;
                }
            }

            Ok(json!({
                "connections": conns,
                "queries": queries,
                "history": history,
                "source": path.display().to_string(),
            }))
        })
        .await
    }
}

/// Whether the source database has a table of this name (an old file may not).
fn has_table(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    let found: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// One stored connection as a client sees it. `spec` is parsed; a spec that is
/// not valid JSON comes back as an empty object rather than failing the whole
/// listing, because one corrupt row must not hide the other connections.
fn conn_json(id: i64, name: String, driver: String, spec: String, created: Option<i64>) -> Json {
    let spec: Json = serde_json::from_str(&spec).unwrap_or_else(|_| json!({}));
    json!({ "id": id, "name": name, "driver": driver, "spec": spec, "created": created })
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store opened in a temporary data dir, so the test never touches the
    /// real one. `LODE_DATA_DIR` is the documented override, which is exactly
    /// what makes this testable.
    fn temp_store(tag: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lode-store-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lode.db");
        let conn = Connection::open(&path).unwrap();
        Store::apply_schema(&conn).unwrap();
        (
            Store {
                conn: Arc::new(Mutex::new(conn)),
                path,
            },
            dir,
        )
    }

    #[tokio::test]
    async fn connections_round_trip_and_upsert() {
        let (s, _dir) = temp_store("conns");
        s.save_connection("prod".into(), "postgres".into(), json!({ "params": { "host": "h" } }))
            .await
            .unwrap();
        s.save_connection("prod".into(), "postgres".into(), json!({ "params": { "host": "h2" } }))
            .await
            .unwrap();
        let all = s.list_connections().await.unwrap();
        assert_eq!(all.len(), 1, "a re-save must UPDATE, not duplicate");
        let got = s.get_connection("prod".into()).await.unwrap().unwrap();
        assert_eq!(got["spec"]["params"]["host"], "h2");
        assert!(s.remove_connection("prod".into()).await.unwrap());
        assert!(s.get_connection("prod".into()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn queries_are_scoped_per_connection() {
        let (s, _dir) = temp_store("queries");
        s.save_query("a".into(), "top".into(), "SELECT 1".into()).await.unwrap();
        s.save_query("b".into(), "top".into(), "SELECT 2".into()).await.unwrap();
        s.save_query("a".into(), "top".into(), "SELECT 3".into()).await.unwrap();
        assert_eq!(s.list_queries("a".into()).await.unwrap().len(), 1);
        assert_eq!(
            s.get_query("a".into(), "top".into()).await.unwrap().unwrap()["sql"],
            "SELECT 3"
        );
        assert_eq!(
            s.get_query("b".into(), "top".into()).await.unwrap().unwrap()["sql"],
            "SELECT 2"
        );
    }

    #[tokio::test]
    async fn history_is_newest_first_and_capped() {
        let (s, _dir) = temp_store("history");
        for i in 0..5 {
            s.record_history(
                Some("c".into()),
                Some("sqlite".into()),
                format!("SELECT {i}"),
                Some("done".into()),
                Some(i),
                Some(1),
            )
            .await
            .unwrap();
        }
        let got = s.list_history(Some(3)).await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0]["statement"], "SELECT 4");
    }

    /// The migration, against a file written in the OLD Lua shape — and run
    /// TWICE, because a client is allowed to call it on every start.
    #[tokio::test]
    async fn import_is_additive_and_idempotent() {
        let (s, dir) = temp_store("import");
        let legacy = dir.join("legacy.db");
        {
            let old = Connection::open(&legacy).unwrap();
            old.execute_batch(
                r#"
                CREATE TABLE connections (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE,
                    driver TEXT NOT NULL, spec TEXT NOT NULL, created INTEGER);
                CREATE TABLE queries (id INTEGER PRIMARY KEY AUTOINCREMENT, conn TEXT NOT NULL,
                    name TEXT NOT NULL, sql TEXT NOT NULL, updated_at INTEGER);
                CREATE TABLE history (id INTEGER PRIMARY KEY AUTOINCREMENT, conn TEXT, driver TEXT,
                    statement TEXT NOT NULL, state TEXT, ms INTEGER, "rows" INTEGER, ts INTEGER);
                INSERT INTO connections (name, driver, spec, created)
                    VALUES ('old', 'sqlite', '{"params":{"file":"/tmp/x.db"}}', 111);
                INSERT INTO queries (conn, name, sql, updated_at) VALUES ('old', 'q1', 'SELECT 1', 222);
                INSERT INTO history (conn, driver, statement, state, ms, "rows", ts)
                    VALUES ('old', 'sqlite', 'SELECT 1', 'done', 3, 1, 333);
                "#,
            )
            .unwrap();
        }

        let first = s.import_from(legacy.clone()).await.unwrap();
        assert_eq!(first["connections"], 1);
        assert_eq!(first["queries"], 1);
        assert_eq!(first["history"], 1);

        let second = s.import_from(legacy).await.unwrap();
        assert_eq!(second["connections"], 0, "a second import must add nothing");
        assert_eq!(second["queries"], 0);
        assert_eq!(second["history"], 0);

        // And the imported spec survived as a parsed object, not a string.
        let got = s.get_connection("old".into()).await.unwrap().unwrap();
        assert_eq!(got["spec"]["params"]["file"], "/tmp/x.db");
        assert_eq!(got["created"], 111);
    }
}
