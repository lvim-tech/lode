// lode/driver: the extensible driver architecture — the centerpiece.
//
// A new DB type is added by writing ONE `drivers/<kind>.rs` that impls `Driver`
// (+ its `Connection` and `ResultStream`) and registering it in registry.rs
// behind a cargo feature. Nothing in the core, the RPC layer, or the Lua side
// changes: the Lua UI discovers driver kinds and builds each connection form
// from the `DriverMeta` returned by `rpc.hello`.
//
// Every driver — SQL or not — presents results as uniform tabular PAGES of
// `Value`, so the result grid renders them identically. Sync engines (sqlite,
// duckdb, oracle) live behind the same async traits via spawn_blocking.

use async_trait::async_trait;

use crate::net::NetContext;
use crate::spec::{
    Column, ConnSpec, DriverMeta, Index, Node, ObjRef, RowOutcome, RowPlan, TableColumn, Value,
};

/// A registered database type. Immutable, cheap; holds the static metadata and
/// knows how to open a connection.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Static metadata (kind, form params, accepted auth, capabilities).
    fn meta(&self) -> &'static DriverMeta;

    /// Open one connection. `net` has already resolved any SSH tunnel, so the
    /// driver dials the address `net` hands it — tunnelling is transparent.
    async fn connect(&self, spec: &ConnSpec, net: NetContext) -> anyhow::Result<Box<dyn Connection>>;

    /// Why this statement is destructive, or `None` when it is not.
    ///
    /// A client asks before running one. It cannot decide this itself: what
    /// `DROP` means, whether a `delete` with an empty filter removes everything,
    /// and whether `FLUSHALL` is a word at all are facts about the LANGUAGE, and
    /// a client that knew them would be a list of driver names in a client.
    ///
    /// The default covers SQL, which is what most drivers are. A driver whose
    /// statements are not SQL overrides it; one whose statements are SQL with a
    /// dialect's own destructive verbs extends it.
    ///
    /// The answer is a REASON, not a flag, because "this will delete every row
    /// in the table" is what makes someone stop and "dangerous: true" is not.
    fn classify(&self, statement: &str) -> Option<String> {
        default_sql_classify(statement)
    }

    /// Whether this statement CHANGES anything.
    ///
    /// Deliberately a different question from `classify`. That one asks "is
    /// this dangerous", and an `INSERT` is not — it is still a write, and a
    /// connection marked read-only has to refuse it. Conflating the two would
    /// make "read only" mean "no DROP", which is not what anybody means by it.
    ///
    /// Defaults to SQL, which is what most drivers are.
    fn writes(&self, statement: &str) -> bool {
        default_sql_writes(statement)
    }
}

/// Does this SQL change anything? Reads the leading verb of each statement in
/// the batch; anything it does not recognise counts as a write, because the
/// cost of being wrong runs one way only — refusing a read somebody wanted is
/// an annoyance, and allowing a write they forbade is the thing read-only
/// exists to stop.
pub fn default_sql_writes(statement: &str) -> bool {
    let cleaned: String = statement
        .lines()
        .map(|l| match l.find("--") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join(" ");
    cleaned
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|s| {
            let first = s.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            // A PRAGMA that ASSIGNS changes the database; one that asks does
            // not. `SET` is left out of the reading list entirely — `SET GLOBAL`
            // changes a server, and telling the two apart is a parser's job.
            if first == "pragma" {
                return s.contains('=');
            }
            !matches!(
                first.as_str(),
                "select" | "with" | "show" | "describe" | "desc" | "explain" | "values" | "table"
                    | "use"
            )
        })
}

#[cfg(test)]
mod writes_tests {
    use super::default_sql_writes;

    #[test]
    fn reading_is_not_writing() {
        for s in [
            "SELECT * FROM t",
            "  with x as (select 1) select * from x",
            "EXPLAIN SELECT 1",
            "SHOW TABLES",
            "PRAGMA table_info('t')",
        ] {
            assert!(!default_sql_writes(s), "{s} does not change anything");
        }
    }

    #[test]
    fn writing_is_writing_even_when_it_is_not_dangerous() {
        // The point of the distinction: an INSERT is not dangerous and IS a
        // write, and read-only has to refuse it.
        for s in ["INSERT INTO t VALUES (1)", "CREATE TABLE t(a)", "PRAGMA journal_mode = WAL"] {
            assert!(default_sql_writes(s), "{s} changes something");
        }
    }

    /// A batch is a write if ANY statement in it is: hiding an UPDATE behind a
    /// leading SELECT is the obvious way past a check that reads only the first
    /// verb.
    #[test]
    fn one_write_in_a_batch_makes_the_batch_a_write() {
        assert!(default_sql_writes("SELECT 1; UPDATE t SET a = 1"));
    }

    /// Anything unrecognised counts as a write. Being wrong costs an annoyance
    /// one way and a forbidden write the other.
    #[test]
    fn an_unknown_verb_is_assumed_to_write() {
        assert!(default_sql_writes("VACUUM"));
        assert!(default_sql_writes("CALL do_something()"));
    }

    #[test]
    fn a_commented_out_update_is_not_one() {
        assert!(!default_sql_writes("SELECT 1 -- UPDATE t SET a = 1"));
    }
}

/// The destructive-statement rule for SQL, shared by every SQL driver.
///
/// Deliberately conservative and deliberately dumb: it reads the leading verb
/// and looks for a `WHERE`. A parser would be better and is a parser; what this
/// has to do is never stay silent about `DELETE FROM t`, and never cry wolf so
/// often that the confirmation becomes a reflex.
pub fn default_sql_classify(statement: &str) -> Option<String> {
    // Strip line comments before looking, so a commented-out WHERE is not read
    // as one and a verb below a comment line is still the verb.
    let cleaned: String = statement
        .lines()
        .map(|l| match l.find("--") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let lower = cleaned.to_ascii_lowercase();
    let t = lower.trim_start();
    let first = t.split_whitespace().next().unwrap_or("");
    let has_where = lower.contains(" where ") || lower.ends_with(" where");

    match first {
        "drop" => Some(format!(
            "DROP removes the object and everything in it{}",
            object_of(t, 1)
        )),
        "truncate" => Some(format!("TRUNCATE empties the table{}", object_of(t, 1))),
        "delete" if !has_where => Some("DELETE with no WHERE clause removes EVERY row".into()),
        "update" if !has_where => Some("UPDATE with no WHERE clause rewrites EVERY row".into()),
        "alter" if lower.contains(" drop ") => {
            Some("ALTER … DROP removes a column or constraint and its data".into())
        }
        "grant" | "revoke" => Some("this changes who may do what".into()),
        _ => None,
    }
}

/// The word after position `skip`, rendered as " (name)" — a best effort at
/// naming what is about to go, and silent when it cannot.
fn object_of(lowered: &str, skip: usize) -> String {
    let words: Vec<&str> = lowered.split_whitespace().skip(skip).collect();
    // `DROP TABLE x`, `TRUNCATE TABLE x`, `TRUNCATE x`.
    let name = words
        .iter()
        .find(|w| !matches!(**w, "table" | "view" | "index" | "if" | "exists" | "database" | "schema"));
    match name {
        Some(n) => format!(" — {}", n.trim_end_matches(';')),
        None => String::new(),
    }
}

/// A live connection. Held server-side behind a mutex and addressed by a numeric
/// `conn_id`; mutable so stateful drivers (a current database, a session) work.
/// What a live view answered.
///
/// `acts` is the rows the view's ACTION applies to, by key. Empty means all of
/// them, which is what a view whose every row is a session wants.
///
/// It exists because `currentOp` on an idle MongoDB is mostly the server's own
/// housekeeping — `JournalFlusher`, `NoopWriter`, the oplog applier — and
/// those are running, so a view called "what is running" must show them. What
/// it must not do is offer to kill them. Which rows are somebody's work and
/// which are the server's is DRIVER knowledge, so the driver says.
pub struct Inspected {
    pub columns: Vec<crate::spec::Column>,
    pub rows: Vec<Vec<crate::spec::Value>>,
    pub acts: Vec<String>,
}

impl Inspected {
    /// A view where every row can be acted on.
    pub fn all(
        columns: Vec<crate::spec::Column>,
        rows: Vec<Vec<crate::spec::Value>>,
    ) -> Self {
        Inspected { columns, rows, acts: Vec::new() }
    }
}

#[async_trait]
pub trait Connection: Send {
    /// Databases visible on this connection (for `MULTI_DB` drivers).
    async fn databases(&mut self) -> anyhow::Result<Vec<String>>;

    /// Switch the active database.
    async fn switch_database(&mut self, db: &str) -> anyhow::Result<()>;

    /// The schema → object tree (schemas, tables, views, collections …).
    async fn structure(&mut self) -> anyhow::Result<Vec<Node>>;

    /// The columns of one object, each flagged when it is part of the identifying key.
    async fn columns(&mut self, obj: &ObjRef) -> anyhow::Result<Vec<TableColumn>>;

    /// The indexes on one object (`Caps::indexes`). Defaults to "this engine has none to show" so a driver
    /// opts IN by implementing it — the same shape as `cancel_token` — rather than every driver being forced
    /// to write a stub. A driver that implements this MUST also set `Caps::indexes`, since the UI offers the
    /// helper off the capability, not off a probe.
    async fn indexes(&mut self, _obj: &ObjRef) -> anyhow::Result<Vec<Index>> {
        Ok(Vec::new())
    }

    /// The CREATE statement for one object (`Caps::ddl`), or `None` when the engine has no server-side way to
    /// produce it. Returning the engine's OWN answer is the whole point: re-assembling a CREATE from column
    /// metadata would silently drop constraints, defaults and storage clauses, and would read as authoritative
    /// while being wrong — so a driver that cannot ask the server for it does not claim the capability.
    async fn ddl(&mut self, _obj: &ObjRef) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// The statement a row write WOULD run, for showing before it does.
    ///
    /// Separate from doing it so that a person sees the real thing — not a
    /// client's guess at what the driver might build, which is the version that
    /// is reassuring and wrong.
    async fn plan_row(&mut self, _plan: &RowPlan) -> anyhow::Result<String> {
        anyhow::bail!("this engine cannot edit a row from the grid")
    }

    /// Change or remove ONE row.
    ///
    /// The contract a driver takes on by implementing this is not "build an
    /// UPDATE" — it is:
    ///
    /// * every value is BOUND as a parameter, never pasted into the statement,
    ///   so a value cannot become syntax;
    /// * the write happens inside a transaction, and if it turns out to have
    ///   touched anything other than the single addressed row, it is ROLLED
    ///   BACK and the call fails. Being wrong about which rows a statement hits
    ///   is the whole risk here, and `affected: 3` reported after the fact is a
    ///   worse outcome than a refusal.
    ///
    /// A driver that cannot promise both does not implement this and does not
    /// claim `Caps::edit`.
    async fn write_row(&mut self, _plan: &RowPlan) -> anyhow::Result<RowOutcome> {
        anyhow::bail!("this engine cannot edit a row from the grid")
    }

    /// What this engine says about itself (`Caps::metrics`).
    ///
    /// Defaults to nothing, so a driver opts IN. An engine that cannot be asked
    /// — or is not allowed to answer, which a hosted service often is not —
    /// says nothing rather than having numbers invented for it from the client
    /// side, where they would be guesses wearing the server's name.
    async fn metrics(&mut self) -> anyhow::Result<Vec<crate::spec::Metric>> {
        Ok(Vec::new())
    }

    /// One of the live views this driver declared in `meta().inspectors`.
    ///
    /// A table, not a list of numbers: "who is connected" has a row per
    /// session with a dozen things to say about each, and squeezing that into
    /// `Metric { group, name, value }` would lose the row.
    ///
    /// Default: nothing. A driver that declares no inspectors is never asked,
    /// so this is only reached by a driver that declared one and did not
    /// implement it — which is a bug in that driver and says so.
    async fn inspect(
        &mut self,
        name: &str,
    ) -> anyhow::Result<Inspected> {
        anyhow::bail!("this driver has no live view called '{name}'")
    }

    /// Act on one row of a live view: terminate a session, and nothing else so
    /// far.
    ///
    /// `key` is what the view itself said identifies the row — a backend pid,
    /// a connection id, an opid. The DRIVER decides what acting means, because
    /// `pg_terminate_backend`, `KILL CONNECTION`, `killOp` and `CLIENT KILL`
    /// are four different statements and none of them is composable from the
    /// others.
    ///
    /// Returns what happened, as a sentence for a person: this is reported
    /// rather than counted, because "it did nothing" and "it killed the
    /// session" need telling apart and a boolean cannot.
    async fn act(&mut self, name: &str, _key: &str) -> anyhow::Result<String> {
        anyhow::bail!("nothing can be done to a row of '{name}'")
    }

    /// Execute a statement, returning a paged result stream.
    async fn execute(&mut self, stmt: &str) -> anyhow::Result<Box<dyn ResultStream>>;

    /// A protocol-level cancel handle for a running query, if the driver supports
    /// one (`Caps::cancel`). It uses a SEPARATE channel to the server, so it can
    /// cancel an in-flight statement even while this connection is busy. `None`
    /// on drivers with no cancellation.
    fn cancel_token(&self) -> Option<Box<dyn CancelHandle>> {
        None
    }

    /// Whether the link to the server is ENCRYPTED (native TLS negotiated, or the
    /// connection rides an SSH tunnel). Surfaced to the client so an unencrypted
    /// link is never silently accepted. Default false (embedded/local engines).
    fn encrypted(&self) -> bool {
        false
    }

    /// Close the connection, releasing its resources.
    async fn close(self: Box<Self>) -> anyhow::Result<()>;
}

/// A detachable cancel handle for one connection's running query. Held by the
/// daemon's call registry so `query.cancel` can stop a statement mid-flight.
#[async_trait]
pub trait CancelHandle: Send + Sync {
    async fn cancel(&self) -> anyhow::Result<()>;
}

/// A uniform, paged result. `columns()` is known once the stream is opened;
/// `next_page(n)` pulls up to `n` more rows (None = exhausted). The daemon
/// buffers pulled rows so the UI's pagination band can page backwards too.
#[async_trait]
pub trait ResultStream: Send {
    fn columns(&self) -> &[Column];

    /// The affected-row count for a non-query statement (INSERT/UPDATE/DDL), if
    /// the driver reports one and there are no result rows.
    fn affected(&self) -> Option<u64> {
        None
    }

    /// A total row count known UP FRONT — without exhausting the stream — when the driver can
    /// supply one cheaply (e.g. a MongoDB `count_documents` for a find). `None` ⇒ the total is
    /// only known once the stream is exhausted (the server falls back to the buffered length).
    fn total(&self) -> Option<usize> {
        None
    }

    /// Pull up to `n` more rows. `Ok(None)` once the stream is exhausted.
    async fn next_page(&mut self, n: usize) -> anyhow::Result<Option<Vec<Vec<Value>>>>;
}

#[cfg(test)]
mod tests {
    use super::default_sql_classify as c;

    /// The shape that matters: a statement that destroys must never pass
    /// silently, and an ordinary one must never ask — a confirmation that fires
    /// on everything is a reflex, not a safeguard.
    #[test]
    fn sql_destruction_is_named_and_reading_is_not() {
        for (stmt, needle) in [
            ("DROP TABLE users", "users"),
            ("drop table if exists public.t;", "public.t"),
            ("TRUNCATE TABLE events", "events"),
            ("DELETE FROM t", "EVERY row"),
            ("UPDATE t SET a = 1", "EVERY row"),
            ("ALTER TABLE t DROP COLUMN c", "ALTER"),
            ("GRANT ALL ON t TO bob", "who may do what"),
        ] {
            let r = c(stmt).unwrap_or_else(|| panic!("{stmt} must be flagged"));
            assert!(r.contains(needle), "{stmt}: {r} should mention {needle}");
        }

        for stmt in [
            "SELECT * FROM users",
            "DELETE FROM t WHERE id = 1",
            "UPDATE t SET a = 1 WHERE id = 2",
            "INSERT INTO t VALUES (1)",
            "  select 1  ",
            "EXPLAIN DELETE FROM t",
        ] {
            assert!(c(stmt).is_none(), "{stmt} must NOT ask");
        }
    }

    /// A WHERE hidden in a comment is not a WHERE, and a verb hidden behind one
    /// is still the verb.
    #[test]
    fn comments_do_not_hide_the_verb_or_invent_a_where() {
        assert!(c("-- cleanup
DELETE FROM t").is_some(), "the verb survives a comment");
        assert!(
            c("DELETE FROM t -- WHERE id = 1").is_some(),
            "a WHERE inside a comment must not count as one"
        );
        assert!(c("DELETE FROM t WHERE id = 1 -- tidy").is_none());
    }

    #[test]
    fn an_unnameable_object_is_still_flagged() {
        // Nothing to name, but the warning must not be lost for want of a noun.
        assert!(c("DROP").is_some());
        assert!(c("TRUNCATE TABLE").is_some());
    }
}
