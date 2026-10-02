// lode/rpc: the transport — newline-delimited JSON on stdio.
//
// One JSON object per line. The Lua side sends REQUESTS; the daemon sends a
// RESPONSE per request (correlated by `id`) plus unsolicited NOTIFICATIONS
// (no id) for query state and logs. This is a minimal, OURS-only protocol (the
// only clients are ours), so it stays deliberately small rather than full
// JSON-RPC 2.0 envelope machinery.
//
//   request       {"id":N,"method":"…","params":{…}}
//   response ok   {"id":N,"ok":true,"result":{…}}
//   response err  {"id":N,"ok":false,"error":"…"}
//   notification  {"method":"…","params":{…}}

use serde::Deserialize;
use serde_json::json;

/// The protocol version. A client accepts any engine reporting `proto >=` its
/// own minimum (the lvim-fuzzy ABI-min discipline applied to the RPC protocol),
/// so a newer engine stays compatible with an older client as long as this only
/// grows ADDITIVELY.
///
/// 1 — the original set: `rpc.hello`, `conn.*`, `schema.*`, `query.*`.
/// 2 — added the `store.*` family: the saved connections, saved queries and
///     history that used to live inside the client, moved into the engine so
///     two clients share one set. Nothing from 1 changed shape, so a client
///     written against 1 keeps working against a 2 engine.
/// 3 — added `hint`, `browse` and `browse_by_kind` to each `DriverMeta`, so a
///     client can show an empty editor the right LANGUAGE and turn a click on a
///     table into a statement that engine understands. A client that does not
///     read them is unaffected; one that does falls back where they are absent.
pub const PROTO: u32 = 3;

/// One decoded incoming request.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Serialize a successful response line.
pub fn response_ok(id: u64, result: serde_json::Value) -> String {
    json!({ "id": id, "ok": true, "result": result }).to_string()
}

/// Serialize an error response line.
pub fn response_err(id: u64, error: &str) -> String {
    json!({ "id": id, "ok": false, "error": error }).to_string()
}

/// Serialize a notification line (no id).
pub fn notification(method: &str, params: serde_json::Value) -> String {
    json!({ "method": method, "params": params }).to_string()
}
