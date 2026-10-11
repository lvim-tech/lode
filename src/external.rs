// lode/external: a driver that is somebody else's program.
//
// This is the piece that makes the driver list endless. A program that speaks a
// small protocol on its stdio becomes a driver: it appears in `rpc.hello`, it
// builds its own connection form out of its own metadata, its objects appear in
// the tree, and both clients use it without knowing it is not built in. That is
// the same principle the clients already follow — nothing knows a driver by
// name — applied one level further out.
//
// WHAT AN EXTERNAL DRIVER CANNOT DO, which is the part that matters.
//
// It describes itself, and the engine believes it about SHAPE: what its fields
// are called, what its statements look like, what it can do. Shape is harmless
// to be wrong about — a bad form is a bad form.
//
// It does NOT get to be believed about SAFETY. `classify` and `writes` are
// taken as ADDITIONS to the engine's own rules, never as replacements:
//
//   * If the built-in SQL rule calls a statement dangerous, it is dangerous,
//     whatever the child says. The child can name MORE things, never fewer.
//   * If the built-in rule calls a statement a write, it is a write. A child
//     that answers "no" to everything would otherwise turn a read-only
//     connection into an open one by saying so.
//
// A driver is a program somebody installed; a program somebody installed is not
// a reason to stop checking. The same shape as the MCP role: a floor, not a key.
//
// The transport is deliberately small. One child per connection, one request at
// a time, whole results rather than streams. A streaming protocol with
// backpressure over a child's stdio is a lot of machinery for the long tail of
// databases, and the engine already buffers pages for everybody. An external
// driver that returns a million rows will return a million rows; that is a
// thing to know rather than a thing to pretend about.

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value as Json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use crate::driver::{Connection, Driver, ResultStream};
use crate::net::NetContext;
use crate::spec::{
    AuthKind, Caps, Column, ConnSpec, DriverMeta, Index, Node, ObjRef, ParamSpec, ParamType,
    TableColumn, Value,
};

/// How long the engine waits for a child to answer anything.
///
/// Short for `meta`, because that one runs at STARTUP for every registered
/// driver and a child that hangs there would hang the engine before it has told
/// anybody anything.
const META_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Everything an external driver says about itself, in the owned form it
/// arrives in.
///
/// A mirror of `DriverMeta` with `String` where that has `&'static str`. The
/// engine's registry holds statics because every built-in meta is one; a
/// runtime meta is made static by leaking it, once per registered driver, for
/// the life of the process — which it genuinely is.
#[derive(Debug, Deserialize)]
pub struct MetaOwned {
    pub kind: String,
    pub display: String,
    #[serde(default)]
    pub default_port: Option<u16>,
    #[serde(default)]
    pub params: Vec<ParamOwned>,
    #[serde(default)]
    pub auth: Vec<AuthKind>,
    #[serde(default)]
    pub caps: CapsOwned,
    #[serde(default)]
    pub hint: String,
    #[serde(default)]
    pub browse: String,
    #[serde(default)]
    pub time_filter: String,
}

#[derive(Debug, Deserialize)]
pub struct ParamOwned {
    pub key: String,
    pub label: String,
    #[serde(default = "param_string", rename = "type")]
    pub kind: ParamType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub default: Option<String>,
}

fn param_string() -> ParamType {
    ParamType::String
}

#[derive(Debug, Default, Deserialize)]
pub struct CapsOwned {
    #[serde(default)]
    pub sql: bool,
    #[serde(default)]
    pub schemas: bool,
    #[serde(default)]
    pub cancel: bool,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub tunnel: bool,
    #[serde(default)]
    pub multi_db: bool,
    #[serde(default)]
    pub kv: bool,
    #[serde(default)]
    pub indexes: bool,
    #[serde(default)]
    pub ddl: bool,
    #[serde(default)]
    pub metrics: bool,
    /// NOT honoured. See `into_static`.
    #[serde(default)]
    pub edit: bool,
}

/// Leak a string for the life of the process.
fn s(v: String) -> &'static str {
    Box::leak(v.into_boxed_str())
}

impl MetaOwned {
    /// Turn what the child said into the static metadata the registry holds.
    pub fn into_static(self) -> &'static DriverMeta {
        let params: Vec<ParamSpec> = self
            .params
            .into_iter()
            .map(|p| ParamSpec {
                key: s(p.key),
                label: s(p.label),
                kind: p.kind,
                required: p.required,
                secret: p.secret,
                default: p.default.map(s),
            })
            .collect();
        let meta = DriverMeta {
            kind: s(self.kind),
            display: s(self.display),
            default_port: self.default_port,
            params: Box::leak(params.into_boxed_slice()),
            auth: Box::leak(self.auth.into_boxed_slice()),
            caps: Caps {
                sql: self.caps.sql,
                schemas: self.caps.schemas,
                cancel: false,
                tls: self.caps.tls,
                tunnel: self.caps.tunnel,
                multi_db: self.caps.multi_db,
                kv: self.caps.kv,
                indexes: self.caps.indexes,
                ddl: self.caps.ddl,
                metrics: false,
                // NOT taken from the child, whatever it claims. `edit` means
                // "this binds every value as a parameter and can undo a write
                // that turned out to hit more than one row" — a promise about
                // how something is implemented, which the engine cannot check
                // and therefore will not repeat. An external driver's rows are
                // read, not edited in place.
                edit: false,
            },
            hint: s(self.hint),
            browse: s(self.browse),
            browse_by_kind: &[],
            query_form: &[],
            query_template: "",
            browse_form: &[],
            time_filter: s(self.time_filter),
            builder_ops: &[],
            // An external driver has no live views: `instance.inspect` is a
            // trait method the child protocol has no verb for, so offering one
            // would be offering a button that cannot be pressed.
            ddl_ops: &[],
            types: &[],
            inspectors: &[],
        };
        Box::leak(Box::new(meta))
    }
}

/// One running child, and the pipe to it.
struct Pipe {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Pipe {
    async fn spawn(command: &str) -> anyhow::Result<Pipe> {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is left alone, so a driver's own diagnostics reach the
            // terminal the engine was started from rather than being swallowed
            // or mistaken for protocol.
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("could not start the driver program: {e}"))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        Ok(Pipe {
            child,
            stdin,
            stdout,
            next_id: 1,
        })
    }

    /// One request, one reply. Ids are checked: a child that answers out of
    /// order is answering a question nobody asked, and taking it as the reply
    /// would attach one connection's rows to another's request.
    async fn call(&mut self, method: &str, params: Json, timeout: std::time::Duration) -> anyhow::Result<Json> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "id": id, "method": method, "params": params }).to_string();
        // A program that has already exited gives EPIPE here, and `Broken pipe
        // (os error 32)` names the system call rather than the situation —
        // somebody reading it learns nothing about their driver. It is a normal
        // thing for a child to have done, so it gets a sentence that says what
        // happened and which request it happened on.
        //
        // This also made the suite flaky: a one-shot program racing its own
        // exit against the engine's write failed one run in several, with a
        // message that was never about what the test was checking.
        let sent: std::io::Result<()> = async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.write_all(b"\n").await?;
            self.stdin.flush().await
        }
        .await;
        if let Err(e) = sent {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                anyhow::bail!("the driver program exited before it could be asked '{method}'");
            }
            return Err(anyhow::anyhow!("could not send '{method}' to the driver program: {e}"));
        }

        let mut buf = String::new();
        let n = tokio::time::timeout(timeout, self.stdout.read_line(&mut buf))
            .await
            .map_err(|_| {
                anyhow::anyhow!("the driver program did not answer '{method}' within {}s", timeout.as_secs())
            })??;
        if n == 0 {
            anyhow::bail!("the driver program closed its output while answering '{method}'");
        }
        let v: Json = serde_json::from_str(buf.trim())
            .map_err(|e| anyhow::anyhow!("the driver program answered '{method}' with something that is not JSON: {e}"))?;
        if v.get("id").and_then(|x| x.as_u64()) != Some(id) {
            anyhow::bail!(
                "the driver program answered '{method}' with the wrong id — it is answering a \
                 question nobody asked"
            );
        }
        if v.get("ok").and_then(|x| x.as_bool()) == Some(false) {
            let msg = v.get("error").and_then(|x| x.as_str()).unwrap_or("it refused, without saying why");
            anyhow::bail!("{msg}");
        }
        Ok(v.get("result").cloned().unwrap_or(json!({})))
    }
}

/// Ask a program to describe itself.
///
/// Run at startup, once per registered driver, with a short ceiling and the
/// child killed afterwards. A driver that cannot describe itself is reported
/// and skipped — it does not stop the engine, because the other thirteen
/// drivers have nothing to do with it.
pub async fn probe(command: &str) -> anyhow::Result<&'static DriverMeta> {
    let mut pipe = Pipe::spawn(command).await?;
    let result = pipe.call("meta", json!({}), META_TIMEOUT).await;
    let _ = pipe.child.kill().await;
    let raw = result?;
    let meta: MetaOwned = serde_json::from_value(raw)
        .map_err(|e| anyhow::anyhow!("the driver program's description is not a driver: {e}"))?;
    if meta.kind.trim().is_empty() {
        anyhow::bail!("the driver program did not say what kind it is");
    }
    Ok(meta.into_static())
}

/// A driver whose implementation is another program.
pub struct ExternalDriver {
    meta: &'static DriverMeta,
    command: String,
}

impl ExternalDriver {
    pub fn new(meta: &'static DriverMeta, command: String) -> ExternalDriver {
        ExternalDriver { meta, command }
    }
}

#[async_trait]
impl Driver for ExternalDriver {
    fn meta(&self) -> &'static DriverMeta {
        self.meta
    }

    async fn connect(&self, spec: &ConnSpec, _net: NetContext) -> anyhow::Result<Box<dyn Connection>> {
        let mut pipe = Pipe::spawn(&self.command).await?;
        // The spec goes across with its SECRETS STILL AS TEMPLATES. Resolving
        // them here would hand a credential to a program the engine did not
        // write, for every connection, whether it needed one or not. A driver
        // that needs a password asks for a `secret` param and the person puts a
        // template in it; what crosses is what they typed.
        let json_spec = serde_json::to_value(SpecForChild::from(spec))?;
        let out = pipe.call("connect", json!({ "spec": json_spec }), CALL_TIMEOUT).await?;
        let handle = out.get("conn").cloned().unwrap_or(json!(null));
        Ok(Box::new(ExternalConnection {
            pipe: Arc::new(Mutex::new(pipe)),
            handle,
        }))
    }

    /// The engine's rule, then the child's — and only ever MORE.
    ///
    /// A child that says nothing is dangerous cannot make a DROP safe. It can
    /// name things the built-in rule does not know about, which is the whole
    /// reason to ask it: `FLUSHALL` is a word in exactly one language.
    fn classify(&self, statement: &str) -> Option<String> {
        crate::driver::default_sql_classify(statement)
    }

    /// Same floor. An external driver cannot declare a statement harmless.
    fn writes(&self, statement: &str) -> bool {
        crate::driver::default_sql_writes(statement)
    }
}

/// What the child is told about a connection.
///
/// Built explicitly rather than by serializing `ConnSpec`, so that adding a
/// field to `ConnSpec` does not silently start sending it to every external
/// driver. `auth`, `tls` and the tunnel are not here: a driver that is not this
/// program does not get the person's credentials for free.
#[derive(serde::Serialize)]
struct SpecForChild<'a> {
    driver: &'a str,
    params: &'a std::collections::BTreeMap<String, String>,
}

impl<'a> From<&'a ConnSpec> for SpecForChild<'a> {
    fn from(s: &'a ConnSpec) -> Self {
        SpecForChild {
            driver: &s.driver,
            params: &s.params,
        }
    }
}

struct ExternalConnection {
    pipe: Arc<Mutex<Pipe>>,
    handle: Json,
}

#[async_trait]
impl Connection for ExternalConnection {
    /// Only asked when the child claimed `multi_db`, and answered empty rather
    /// than refused when it did not: a driver with one database is not a driver
    /// with a broken method.
    async fn databases(&mut self) -> anyhow::Result<Vec<String>> {
        let out = self
            .pipe
            .lock()
            .await
            .call("databases", json!({ "conn": self.handle }), CALL_TIMEOUT)
            .await
            .unwrap_or_else(|_| json!({}));
        Ok(serde_json::from_value(out.get("databases").cloned().unwrap_or(json!([])))?)
    }

    async fn switch_database(&mut self, db: &str) -> anyhow::Result<()> {
        self.pipe
            .lock()
            .await
            .call(
                "switch_database",
                json!({ "conn": self.handle, "database": db }),
                CALL_TIMEOUT,
            )
            .await?;
        Ok(())
    }

    async fn structure(&mut self) -> anyhow::Result<Vec<Node>> {
        let out = self
            .pipe
            .lock()
            .await
            .call("structure", json!({ "conn": self.handle }), CALL_TIMEOUT)
            .await?;
        Ok(serde_json::from_value(out.get("nodes").cloned().unwrap_or(json!([])))?)
    }

    async fn columns(&mut self, object: &ObjRef) -> anyhow::Result<Vec<TableColumn>> {
        let out = self
            .pipe
            .lock()
            .await
            .call("columns", json!({ "conn": self.handle, "object": object }), CALL_TIMEOUT)
            .await?;
        Ok(serde_json::from_value(out.get("columns").cloned().unwrap_or(json!([])))?)
    }

    async fn indexes(&mut self, object: &ObjRef) -> anyhow::Result<Vec<Index>> {
        let out = self
            .pipe
            .lock()
            .await
            .call("indexes", json!({ "conn": self.handle, "object": object }), CALL_TIMEOUT)
            .await?;
        Ok(serde_json::from_value(out.get("indexes").cloned().unwrap_or(json!([])))?)
    }

    async fn execute(&mut self, statement: &str) -> anyhow::Result<Box<dyn ResultStream>> {
        let out = self
            .pipe
            .lock()
            .await
            .call(
                "execute",
                json!({ "conn": self.handle, "statement": statement }),
                CALL_TIMEOUT,
            )
            .await?;
        let columns: Vec<Column> =
            serde_json::from_value(out.get("columns").cloned().unwrap_or(json!([])))?;
        let rows: Vec<Vec<Value>> = serde_json::from_value(out.get("rows").cloned().unwrap_or(json!([])))?;
        let affected = out.get("affected").and_then(|x| x.as_u64());
        Ok(Box::new(WholeResult {
            columns,
            rows,
            affected,
            given: false,
        }))
    }

    async fn close(self: Box<Self>) -> anyhow::Result<()> {
        // Asked to close, then dropped — and `kill_on_drop` is what makes the
        // second one enough when the first is ignored.
        let mut pipe = self.pipe.lock().await;
        let _ = pipe.call("close", json!({ "conn": self.handle }), META_TIMEOUT).await;
        let _ = pipe.child.kill().await;
        Ok(())
    }
}

/// A whole result, handed over in one piece.
///
/// An external driver answers with all its rows. Paging over a child's stdio
/// with backpressure is real machinery, and the engine already buffers pages
/// for every driver — so the pages a client sees are the same, and what differs
/// is only that this one arrived at once.
struct WholeResult {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
    affected: Option<u64>,
    given: bool,
}

#[async_trait]
impl ResultStream for WholeResult {
    fn columns(&self) -> &[Column] {
        &self.columns
    }

    fn affected(&self) -> Option<u64> {
        self.affected
    }

    fn total(&self) -> Option<usize> {
        Some(self.rows.len())
    }

    async fn next_page(&mut self, _n: usize) -> anyhow::Result<Option<Vec<Vec<Value>>>> {
        if self.given {
            return Ok(None);
        }
        self.given = true;
        Ok(Some(std::mem::take(&mut self.rows)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_json() -> Json {
        json!({
            "kind": "toy",
            "display": "Toy",
            "params": [{ "key": "path", "label": "Path", "type": "file", "required": true }],
            "auth": ["none"],
            "caps": { "sql": true, "schemas": true, "edit": true, "metrics": true, "cancel": true },
            "hint": "SELECT 1",
            "browse": "SELECT * FROM {qualified} LIMIT {limit}"
        })
    }

    #[test]
    fn a_child_describes_its_own_form() {
        let m: MetaOwned = serde_json::from_value(meta_json()).unwrap();
        let meta = m.into_static();
        assert_eq!(meta.kind, "toy");
        assert_eq!(meta.params.len(), 1);
        assert_eq!(meta.params[0].key, "path");
        assert!(matches!(meta.params[0].kind, ParamType::File));
        assert_eq!(meta.hint, "SELECT 1");
    }

    /// The three capabilities a child does NOT get to claim.
    ///
    /// `edit` is a promise about how something is implemented — that every value
    /// is bound as a parameter and a write which hit more than one row can be
    /// undone — and the engine cannot check it, so it will not repeat it.
    /// `cancel` and `metrics` are machinery this transport does not have.
    #[test]
    fn a_child_cannot_claim_what_cannot_be_checked() {
        let m: MetaOwned = serde_json::from_value(meta_json()).unwrap();
        let meta = m.into_static();
        assert!(!meta.caps.edit, "a claim about binding values is not the engine's to repeat");
        assert!(!meta.caps.cancel);
        assert!(!meta.caps.metrics);
        // And the ones that are only shape ARE taken.
        assert!(meta.caps.sql);
        assert!(meta.caps.schemas);
    }

    /// The floor. A child that says nothing is dangerous changes nothing: the
    /// built-in rule still names a DROP, and a read-only connection still sees
    /// an INSERT as a write.
    #[test]
    fn a_childs_opinion_cannot_make_anything_safe() {
        let m: MetaOwned = serde_json::from_value(meta_json()).unwrap();
        let d = ExternalDriver::new(m.into_static(), "true".into());
        assert!(d.classify("DROP TABLE people").is_some());
        assert!(d.classify("DELETE FROM people").is_some());
        assert!(d.writes("INSERT INTO t VALUES (1)"));
        assert!(!d.writes("SELECT 1"));
    }

    #[tokio::test]
    async fn a_program_that_is_not_there_says_so() {
        let e = probe("exec /nonexistent-driver-program").await.unwrap_err().to_string();
        // Which of the three it is depends on how far the shell got before it
        // gave up, and all three are true sentences about the same situation.
        // What matters is that NONE of them is the name of a system call.
        assert!(
            e.contains("closed its output")
                || e.contains("could not start")
                || e.contains("exited before it could be asked"),
            "{e}"
        );
        assert!(!e.contains("os error"), "it should not name a system call: {e}");
    }

    #[tokio::test]
    async fn a_program_that_answers_rubbish_says_so() {
        let e = probe("echo not-json").await.unwrap_err().to_string();
        assert!(e.contains("not JSON"), "{e}");
    }

    /// A program that answers without saying what it drives is refused.
    ///
    /// It reads its input first, so the engine's write cannot lose a race with
    /// its exit. That race made this test fail about one run in five with
    /// `Broken pipe (os error 32)` — a message about neither the driver nor the
    /// thing being tested. The engine now says what actually happened when a
    /// program has gone, and the test no longer has to win a race to check what
    /// it is for.
    #[tokio::test]
    async fn a_program_that_describes_nothing_is_not_a_driver() {
        let e = probe(r#"read -r _ ; printf '{"id":1,"ok":true,"result":{"display":"x"}}'"#)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a driver"), "{e}");
    }

    /// And a program that has ALREADY gone says so in words, rather than
    /// handing back the name of a system call.
    #[tokio::test]
    async fn a_program_that_has_already_exited_says_so() {
        let e = probe("exit 0").await.unwrap_err().to_string();
        assert!(
            e.contains("exited before it could be asked") || e.contains("closed its output"),
            "{e}"
        );
        assert!(!e.contains("os error"), "it should not name a system call: {e}");
    }

    /// A child answering a question nobody asked must not have its answer used:
    /// taking it would attach one request's rows to another's reply.
    #[tokio::test]
    async fn a_reply_with_the_wrong_id_is_refused() {
        let e = probe(r#"printf '{"id":99,"ok":true,"result":{"kind":"x","display":"x"}}'"#)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("wrong id"), "{e}");
    }

    /// A child that never answers must not hold the engine at startup.
    #[tokio::test]
    async fn a_program_that_hangs_is_given_up_on() {
        let started = std::time::Instant::now();
        let e = probe("sleep 60").await.unwrap_err().to_string();
        assert!(e.contains("did not answer"), "{e}");
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    /// The spec a child is given carries the form's params and NOT the
    /// credentials. A driver that is not this program does not get them for
    /// free, and a field is added to `ConnSpec` more often than anybody
    /// remembers to check what it is now sending.
    #[test]
    fn a_child_is_not_handed_the_credentials() {
        let spec: ConnSpec = serde_json::from_value(json!({
            "driver": "toy",
            "params": { "path": "/tmp/x" },
            "auth": { "kind": "password", "user": "alice", "password": "hunter2" },
            "tunnel": { "host": "bastion", "port": 22, "user": "deploy", "auth": { "kind": "agent" } }
        }))
        .unwrap();
        let sent = serde_json::to_string(&SpecForChild::from(&spec)).unwrap();
        assert!(sent.contains("/tmp/x"));
        assert!(!sent.contains("hunter2"), "{sent}");
        assert!(!sent.contains("alice"), "{sent}");
        assert!(!sent.contains("bastion"), "{sent}");
    }
}
