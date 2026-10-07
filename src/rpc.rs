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
/// 4 — added `query.classify`: ask the ENGINE whether a statement is
///     destructive and why, before running it. A client cannot decide this —
///     what DROP means, whether an empty mongo filter deletes everything, and
///     whether FLUSHALL is a word are facts about the language.
/// 5 — added `row.write` and `DriverMeta.caps.edit`: change or remove ONE row
///     by its key. The client sends VALUES, never SQL, and the engine refuses
///     to write at all unless the key is exactly the object's identifying key
///     and the caller passed `confirm` — so a client that forgets to ask
///     cannot write anyway. Additive: an older client never calls it, and a
///     newer client against an older engine sees `edit: false` and offers
///     nothing.
/// 6 — added `instance.metrics` with `caps.metrics`, and `store.charts.*`.
///     The metrics are each driver asking its OWN system views, because what a
///     server will say about itself and what it calls it differ completely
///     between engines. The charts are in the engine's store rather than a
///     client's file, for the same reason saved queries are: a chart somebody
///     made is something they will want from the editor too. Additive — the
///     store's schema version is unchanged for the tables the Lua side knows.
/// 7 — added the AUDIT LOG: `store.audit.list`, and an optional `name` on a
///     ConnSpec so a record can say WHICH connection a statement ran on in the
///     word a person would search for. The log is written by the engine on the
///     paths that change data and on connect/disconnect, so a client cannot
///     leave itself out of it, and there is no method to delete from it. Each
///     row carries a fingerprint of the statement's shape and the statement
///     itself with the credential grammars redacted; `LODE_AUDIT_STATEMENTS`
///     turns the statement column down to the shape alone, or off. Additive: a
///     client that does not send `name` gets rows that name the driver and
///     leave the name empty, and a client that never asks for the log is
///     unaffected — the log is still written.
pub const PROTO: u32 = 7;

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
