// lode/vault: resolve a `{{ vault "name" }}` secret from the lvim-keyring agent, and store one.
//
// The wallet (lvim-keyring) runs its OWN per-user daemon behind a unix socket. This module is a
// one-shot CLIENT of it: connect, send a single `secret.get`, read the value. lode never spawns
// the keyring — the wallet's gate must stay in front of the USER (unlock from the editor), not be
// silently bypassed by a background DB connect. If the agent is down or locked, the error is
// user-actionable, so the connection form's Test/connect explains exactly what to do.
//
// The seam is deliberately tiny: every driver credential field is already a `Secret`, and `Secret::
// resolve()` dispatches this verb — so no driver or spec change is needed for `{{ vault "…" }}`.

use anyhow::{anyhow, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The keyring agent socket: `$LVIM_KEYRING_SOCK`, else `$XDG_RUNTIME_DIR/lvim-keyring/agent.sock`.
fn socket_path() -> Option<String> {
    if let Some(s) = std::env::var("LVIM_KEYRING_SOCK").ok().filter(|s| !s.is_empty()) {
        return Some(s);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok().filter(|s| !s.is_empty())?;
    Some(format!("{runtime}/lvim-keyring/agent.sock"))
}

/// Fetch the secret named `name` from the keyring agent.
pub async fn fetch(name: &str) -> Result<String> {
    let msg = talk("secret.get", serde_json::json!({ "name": name })).await?;
    msg.pointer("/result/value")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("secret: lvim-keyring returned no value"))
}

/// Store `value` under `name`, so a client can stop keeping it itself.
///
/// This is the one verb here that WRITES, and it exists so a password typed
/// into a connection form can be moved into the wallet instead of being
/// written into the store. The value travels the engine's stdin pipe and this
/// socket and is never stored, logged or echoed back — the reply says only
/// whether it landed.
///
/// `wait` is not passed, exactly as in `fetch`: a locked wallet is an error
/// with an instruction in it, not something to unlock from the background. The
/// gate belongs in front of the person.
pub async fn put(name: &str, value: &str) -> Result<()> {
    talk("secret.set", serde_json::json!({ "name": name, "value": value, "wait": false })).await?;
    Ok(())
}

/// One request, one reply. Connect, send, read until OUR id comes back.
async fn talk(method: &str, params: Value) -> Result<Value> {
    let path = socket_path().ok_or_else(|| {
        anyhow!("secret: lvim-keyring socket path is unknown (set $XDG_RUNTIME_DIR or $LVIM_KEYRING_SOCK)")
    })?;

    let stream = UnixStream::connect(&path).await.map_err(|_| {
        anyhow!("secret: lvim-keyring is not running — open Neovim's :LvimKeyring (or install lvim-keyring)")
    })?;

    let req = serde_json::json!({ "id": 1, "method": method, "params": params }).to_string();
    let mut reader = BufReader::new(stream);
    reader.get_mut().write_all(req.as_bytes()).await?;
    reader.get_mut().write_all(b"\n").await?;
    reader.get_mut().flush().await?;

    // Read lines until our response (id == 1); the agent may interleave a `vault.state` notification.
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow!("secret: lvim-keyring closed the connection"));
        }
        let msg: Value = match serde_json::from_str(line.trim()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if msg.get("id").and_then(Value::as_u64) != Some(1) {
            continue; // a notification — skip
        }
        if msg.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(msg);
        }
        let err = msg.get("error").and_then(Value::as_str).unwrap_or("unknown error");
        return Err(if err == "locked" {
            anyhow!("secret: the keyring is locked — :LvimKeyring unlock")
        } else {
            anyhow!("secret: lvim-keyring: {err}")
        });
    }
}
