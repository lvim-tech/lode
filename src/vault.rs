// lode/vault: the lvim-keyring wallet — the SAME wallet the Neovim client uses.
//
// Not a second implementation of anything. One vault file, one unix socket, one
// daemon program, one set of secret names (`db/<connection>`, which is
// lvim-db's convention). A connection saved here with `{{ vault "db/prod" }}`
// resolves in the editor, and one saved there resolves here. That is the whole
// design constraint and everything below follows from it.
//
// THE CONNECTION IS HELD. The daemon locks the vault and exits when its LAST
// client disconnects (`linger = 0`, `persist = false` by default) — which is
// correct for an agent whose clients stay, and fatal for one that opens a
// socket per question. With the editor open it never showed, because the
// editor's connection kept the count above zero; alone, this engine would
// unlock, read one secret, disconnect, and watch the agent lock and quit. So
// one link is opened and kept for as long as the engine runs, exactly as the
// editor does.
//
// SPAWNING IS NOT BYPASSING. An earlier note here said lode must never start
// the keyring, "so the gate stays in front of the user". That conflated the
// process with the gate: the gate is the passphrase, and a daemon that has just
// been started is locked and demands it. What must never happen is lode
// answering that prompt for somebody, and it does not — it carries what a
// person typed and nothing else. Starting a locked agent is how a machine with
// no editor on it gets a wallet at all.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// The oldest wallet this engine can speak to.
///
/// The wallet grows additively, by its own stated discipline, so a NEWER
/// daemon stays usable here. Anything older than this is reported with the
/// number rather than failing in the middle of a verb it does not have.
const PROTO_MIN: u32 = 1;

/// The keyring agent socket: `$LVIM_KEYRING_SOCK`, else
/// `$XDG_RUNTIME_DIR/lvim-keyring/agent.sock`. The same two places the editor
/// looks, in the same order.
fn socket_path() -> Option<String> {
    if let Some(s) = std::env::var("LVIM_KEYRING_SOCK").ok().filter(|s| !s.is_empty()) {
        return Some(s);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok().filter(|s| !s.is_empty())?;
    Some(format!("{runtime}/lvim-keyring/agent.sock"))
}

/// Where the daemon binary might be, in the order to try.
///
/// The same shape as every other probe in this project: an explicit override,
/// then `$PATH`, then beside the binary doing the looking. `$PATH` is the one
/// that matters for sharing — an installed daemon is the deliberate, shared
/// choice, and it is what makes "the editor built it" and "lode built it" the
/// same situation.
fn daemon_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("LVIM_KEYRING_DAEMON").filter(|p| !p.is_empty()) {
        out.push(PathBuf::from(p));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            out.push(dir.join("lvim-keyring-daemon"));
        }
    }
    // Beside this engine: `scripts/build` puts both in `build/`, so a checkout
    // that built the wallet has it next to `loded`.
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            out.push(dir.join("lvim-keyring-daemon"));
        }
    }
    out
}

fn find_daemon() -> Option<PathBuf> {
    daemon_paths().into_iter().find(|p| p.is_file())
}

/// What the wallet said about itself when the link opened.
#[derive(Debug, Clone, Default)]
pub struct Hello {
    pub proto: u32,
    pub locked: bool,
    pub vault_exists: bool,
}

struct Link {
    reader: BufReader<UnixStream>,
    next_id: u64,
    hello: Hello,
}

fn link() -> &'static Mutex<Option<Link>> {
    static LINK: OnceLock<Mutex<Option<Link>>> = OnceLock::new();
    LINK.get_or_init(|| Mutex::new(None))
}

/// Open the link, starting the daemon if nothing is listening.
async fn open() -> Result<Link> {
    let path = socket_path().ok_or_else(|| {
        anyhow!("secret: the lvim-keyring socket path is unknown (set $XDG_RUNTIME_DIR or $LVIM_KEYRING_SOCK)")
    })?;
    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(_) => {
            // Nothing is listening. Start one — LOCKED, which is what a fresh
            // daemon is.
            let bin = find_daemon().ok_or_else(|| {
                anyhow!(
                    "secret: no lvim-keyring agent is running and no \
                     `lvim-keyring-daemon` was found on $PATH, beside this engine, or at \
                     $LVIM_KEYRING_DAEMON. Build it with lode's `scripts/build-wallet`, \
                     or install the lvim-keyring Neovim plugin, which builds the same \
                     program"
                )
            })?;
            spawn(&bin, &path)?;
            wait_for(&path).await?
        }
    };
    let mut l = Link { reader: BufReader::new(stream), next_id: 0, hello: Hello::default() };
    let said = request(&mut l, "rpc.hello", Value::Null).await?;
    let got = said.pointer("/result").cloned().unwrap_or(Value::Null);
    l.hello = Hello {
        proto: got.get("proto").and_then(Value::as_u64).unwrap_or(0) as u32,
        locked: got.get("locked").and_then(Value::as_bool).unwrap_or(true),
        vault_exists: got.get("vault_exists").and_then(Value::as_bool).unwrap_or(false),
    };
    if l.hello.proto < PROTO_MIN {
        return Err(anyhow!(
            "secret: the running lvim-keyring agent speaks protocol {} and this engine needs at \
             least {PROTO_MIN}. It is an older build of the same program — close whatever \
             started it and let a newer one take the socket",
            l.hello.proto
        ));
    }
    Ok(l)
}

/// Start the daemon, detached, on its own socket.
///
/// `setsid` so it is not in this engine's process group: a signal meant for
/// lode must not take the wallet down with it, and the wallet outliving the
/// client is the point of an agent. Its output goes nowhere — anything it has
/// to say about a secret is said over the socket, and a daemon writing to a
/// client's stderr is a daemon writing into a JSON-RPC stream.
fn spawn(bin: &std::path::Path, sock: &str) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    if let Some(dir) = std::path::Path::new(sock).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // NOTHING is passed about WHERE the vault lives, and that is deliberate.
    //
    // The daemon's own fallback is `$XDG_DATA_HOME/nvim/lvim-keyring/
    // keyring.vault`, which is exactly what the editor passes explicitly — the
    // two already agree, and its source says so. Naming it here would be a
    // third place to keep in step, and the first time the three disagreed
    // somebody would have two wallets and no idea which held their password.
    //
    // The `nvim` in that path looks wrong on a machine with no editor. It is
    // the price of ONE vault, and it is the right price: a path that read
    // better and opened a different file would be worse in every way that
    // matters.
    let mut c = std::process::Command::new(bin);
    c.arg("--socket")
        .arg(sock)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and is the whole of what runs
    // between fork and exec.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    c.spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("secret: could not start the lvim-keyring agent: {e}"))
}

/// Wait for the socket to appear, briefly.
///
/// The daemon may also exit 0 immediately — that is what the SECOND instance
/// does when another already holds the lock, and it means somebody else's
/// agent is there to connect to. So this polls the socket rather than the
/// child.
async fn wait_for(path: &str) -> Result<UnixStream> {
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if let Ok(s) = UnixStream::connect(path).await {
            return Ok(s);
        }
    }
    Err(anyhow!("secret: the lvim-keyring agent was started but never answered on {path}"))
}

/// One request on an open link, and the reply with OUR id.
async fn request(l: &mut Link, method: &str, params: Value) -> Result<Value> {
    l.next_id += 1;
    let id = l.next_id;
    let req = serde_json::json!({ "id": id, "method": method, "params": params }).to_string();
    l.reader.get_mut().write_all(req.as_bytes()).await?;
    l.reader.get_mut().write_all(b"\n").await?;
    l.reader.get_mut().flush().await?;
    let mut line = String::new();
    loop {
        line.clear();
        let n = l.reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow!("secret: lvim-keyring closed the connection"));
        }
        let msg: Value = match serde_json::from_str(line.trim()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // A NOTIFICATION — the agent says when it locks and unlocks — is not
        // an answer. Skipped, not treated as one.
        if msg.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if msg.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(msg);
        }
        let err = msg.get("error").and_then(Value::as_str).unwrap_or("unknown error");
        return Err(if err == "locked" {
            anyhow!("secret: the keyring is locked")
        } else {
            anyhow!("secret: lvim-keyring: {err}")
        });
    }
}

/// One call on the HELD link, opening or reopening it as needed.
///
/// A dead link is retried ONCE. The agent idle-locks and exits on its own
/// schedule, so a link that was good an hour ago may be a closed socket now —
/// and that is a reconnect, not a failure to report.
async fn call(method: &str, params: Value) -> Result<Value> {
    let cell = link();
    let mut guard = cell.lock().await;
    if guard.is_none() {
        *guard = Some(open().await?);
    }
    let first = {
        let l = guard.as_mut().expect("just opened");
        request(l, method, params.clone()).await
    };
    match first {
        Ok(v) => Ok(v),
        Err(e) => {
            // Only a BROKEN LINK is retried. "locked" and "no such secret" are
            // answers, and asking them twice says the same thing twice.
            let text = e.to_string();
            if !text.contains("closed the connection") {
                return Err(e);
            }
            *guard = None;
            let mut l = open().await?;
            let v = request(&mut l, method, params).await?;
            *guard = Some(l);
            Ok(v)
        }
    }
}

/// What the wallet says about itself: is there one, and is it open?
pub async fn status() -> Result<Hello> {
    let said = call("vault.status", serde_json::json!({})).await?;
    let got = said.pointer("/result").cloned().unwrap_or(Value::Null);
    let cached = link().lock().await.as_ref().map(|l| l.hello.proto).unwrap_or(0);
    Ok(Hello {
        proto: cached,
        locked: got.get("locked").and_then(Value::as_bool).unwrap_or(true),
        vault_exists: got.get("vault_exists").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Open the wallet with a person's passphrase.
///
/// The passphrase is carried, not kept: it goes to the socket and the string
/// this was handed is the caller's to drop. The engine holds no copy, which is
/// why there is no "remember" here and never will be — remembering it is the
/// agent's job, under its own timeout.
pub async fn unlock(password: &str) -> Result<()> {
    call("vault.unlock", serde_json::json!({ "password": password })).await?;
    Ok(())
}

/// Make a wallet, for a machine that has never had one.
pub async fn create(password: &str) -> Result<()> {
    call("vault.create", serde_json::json!({ "password": password })).await?;
    Ok(())
}

/// Shut the wallet, now.
pub async fn lock() -> Result<()> {
    call("vault.lock", serde_json::json!({})).await?;
    Ok(())
}

/// Fetch the secret named `name`.
pub async fn fetch(name: &str) -> Result<String> {
    let msg = call("secret.get", serde_json::json!({ "name": name })).await?;
    msg.pointer("/result/value")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("secret: lvim-keyring returned no value"))
}

/// Store `value` under `name`, so a client can stop keeping it itself.
///
/// The one verb here that WRITES. The value travels this socket and the
/// engine's stdin and is never stored, logged or echoed back — the reply says
/// only whether it landed.
///
/// `wait` is not passed: a locked wallet is an error with an instruction in
/// it, not something to unlock from the background. The gate belongs in front
/// of the person, and the person is at the other end of a prompt.
pub async fn put(name: &str, value: &str) -> Result<()> {
    call("secret.set", serde_json::json!({ "name": name, "value": value, "wait": false })).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The probe order is the project's usual one, and `$PATH` is in it.
    ///
    /// That last part is what makes "the editor built it" and "lode built it"
    /// the same situation: an installed daemon is found by both, so whichever
    /// one starts first serves the other.
    #[test]
    fn the_daemon_is_looked_for_where_every_other_binary_is() {
        let paths = super::daemon_paths();
        assert!(!paths.is_empty(), "nothing is probed at all");
        // Every candidate is named for the one program there is.
        assert!(
            paths.iter().all(|p| p.ends_with("lvim-keyring-daemon")),
            "{paths:?}"
        );
        // $PATH is searched: with a PATH set, its directories are in there.
        let on_path = std::env::var_os("PATH").map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("lvim-keyring-daemon"))
                .collect::<Vec<_>>()
        });
        if let Some(want) = on_path.filter(|v| !v.is_empty()) {
            assert!(
                want.iter().any(|w| paths.contains(w)),
                "nothing from $PATH is probed: {paths:?}"
            );
        }
    }

    /// The socket is the editor's socket, found the editor's way.
    #[test]
    fn the_socket_is_the_one_the_editor_uses() {
        // An explicit override wins, as it does on the Lua side.
        std::env::set_var("LVIM_KEYRING_SOCK", "/tmp/not-really/agent.sock");
        assert_eq!(super::socket_path().as_deref(), Some("/tmp/not-really/agent.sock"));
        std::env::remove_var("LVIM_KEYRING_SOCK");
        // Otherwise it is under the runtime directory, by the same name.
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/4242");
        assert_eq!(
            super::socket_path().as_deref(),
            Some("/run/user/4242/lvim-keyring/agent.sock")
        );
    }
}
