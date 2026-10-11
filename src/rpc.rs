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
/// 8 — added DASHBOARDS: `store.dashboards.{list,get,save,remove}`, and
///     `DriverMeta.time_filter`. A dashboard is an arrangement of saved charts
///     on a grid of 12 columns, with dividers, text, and one time range they
///     all answer to; it lives in the engine's store for the same reason saved
///     charts do. Its tiles reference a chart BY NAME — an index would move the
///     moment a chart was renamed and the board would go on drawing the wrong
///     one under the old heading. The engine REFUSES a board whose tile names a
///     chart that does not exist, runs past the twelfth column, has no size, or
///     overlaps another, and a time range whose ends are not timestamps or that
///     runs backwards. `time_filter` is how each engine spells a time window,
///     as a template over `{col}`, `{from}` and `{to}`; empty where there is no
///     such thing (Redis). Additive: a client that never asks for a dashboard
///     is unaffected, and an older client simply sees a field it ignores.
/// 9 — added PROFILES and PROXIES: `store.profiles.{list,get,save,remove}`,
///     `ConnSpec.{tunnel_profile, proxy, proxy_profile}`, and a `proxy` stage
///     for `conn.test`. A profile is a reusable piece of network, by name — a
///     bastion is one machine and twenty connections go through it, and written
///     into twenty specs, rotating its key means editing twenty of them. A
///     connection that names a profile which is missing FAILS; it never falls
///     back to a direct link, because a connection whose author asked for a
///     bastion and got a direct link is the one outcome that must not happen.
///     A proxy (SOCKS5 or HTTP CONNECT) sits UNDER everything: the database's
///     dial and the SSH server's dial both go through it, since a proxy that is
///     sometimes bypassed is not a rule. Additive.
/// 10 — added HOOKS and SCRIPTS: `store.scripts.{list,save,remove}`,
///      `script.run`, `script.cancel`, `ConnSpec.hooks`, and the
///      `script.output` / `script.state` notifications. A hook is the person's
///      own code run at one of four moments; a PreConnect hook that fails STOPS
///      the connection, which is the only reason to have one before rather than
///      after, and the other three cannot undo what they come after. Every hook
///      and every script is in the audit log, worked or not. The environment a
///      child gets carries CONTEXT and never a credential — a child's
///      environment is readable from `ps` and `/proc`. Lua runs through the
///      interpreter rather than embedded: `mlua` is not in this lock file and
///      cannot be fetched here, and the difference (no sandbox, no API onto the
///      open connection) is real. Additive.
/// 11 — added ACCESS PROVIDERS: `access.{list,test,import}` and a third profile
///      kind, `access`. The engine reads `~/.aws/config` and
///      `~/.aws/credentials` and offers what it finds, saying of each entry what
///      KIND it is and whether it can actually be used — so a client shows the
///      whole list and marks the ones that will not work, rather than offering
///      eight of which three fail only when picked. Static keys, session tokens,
///      `credential_process` and `source_profile` chains resolve; SSO and
///      AssumeRole are REFUSED by name with what would have to happen instead,
///      because they need to talk to AWS and this engine does not. An import
///      stores the profile's NAME, never its key: the credential stays where
///      the AWS tools rotate it. Additive.
/// 12 — added the MCP SERVER and the APPROVAL QUEUE: `loded --mcp` speaks the
///      Model Context Protocol on stdio, and `approvals.{list,decide}` is the
///      human half. It is the SAME engine — same drivers, same store, same
///      `classify`, same connection policy, same audit log — and what differs
///      only ever restricts: a ROLE (`reader` by default) that cannot grant
///      what a connection refuses, `Policy::Open` read as `Confirm` because
///      "unrestricted" is a decision a person made about themselves, and an
///      agent that CANNOT confirm. A statement `classify` names is not run: it
///      is filed for a person, who answers in their own client. An approval is
///      spent on one statement, and a denial STANDS — otherwise "no" would mean
///      "not this second". Additive.
/// 13 — added EXTERNAL DRIVERS: a program that speaks a small protocol on its
///      stdio becomes a driver. It is registered as a `driver` profile holding
///      a command line, asked to describe itself once at startup, and then
///      appears in `rpc.hello` like any other — its own fields, its own hint,
///      its own form. `hello` also carries `external`: what became of each
///      registered program, so one that failed to describe itself has a reason
///      attached rather than being quietly absent.
///
///      It is believed about SHAPE and not about SAFETY. `classify` and
///      `writes` stay the engine's: a child can make nothing safe, cannot claim
///      `edit` (a promise about binding values that the engine cannot check),
///      cannot take a built-in driver's name, and is not handed the
///      credentials. Additive.
///
/// **14** — `store.profiles.users`: which connections point at a profile, asked
///      WITHOUT removing it. `store.profiles.remove` has always returned the
///      same list, but only once the profile was gone, which makes it a report
///      of damage rather than a warning. A client that wanted to ask first had
///      no way to. Additive: nothing else changed, and a client that does not
///      call it behaves exactly as before.
/// **15** — `store.connections.rename {from, to}` → `{renamed, moved: {queries,
///      charts, dashboards, scripts}}`. A rename used to be the client's own
///      save-under-the-new-name plus remove-the-old, and everything scoped to
///      the connection's NAME — saved queries, charts, dashboards, scripts —
///      stayed behind pointing at a name nothing answered to. One call, one
///      transaction. The audit is deliberately not rewritten: it records what
///      happened under the name in use at the time. Refuses a name already
///      taken, because `save` overwrites by name and would eat the other
///      connection. Additive: a client that does not call it behaves as before.
/// **16** — `store.connections.save` takes an optional `group`, and every
///      connection row carries one. A folder is where a client FILES a
///      connection, and it belongs in the shared store rather than in a
///      client's own file: two clients filing one connection in two places is
///      two answers to a question that has one.
///
///      `group` has THREE states, and all three are needed. Absent leaves the
///      folder alone, `null` takes the connection out of one, and a name files
///      it there. Without the first, a save from a client that knows nothing
///      about folders — an older build, or the Neovim client — would empty the
///      folder as a side effect of saving a password. Additive: a client that
///      never sends it behaves exactly as before, and one that never reads the
///      field sees a flat list.
/// 17. `vault.put {name, value}` stores a secret in the lvim-keyring wallet,
///     so a password typed into a form can be moved out of the store instead
///     of being kept in it. The value travels the engine's stdin pipe, which
///     is the same path a literal credential already takes on `conn.connect`;
///     it is never written to the store, the audit log or an error message.
/// 18. Every result `Column` carries `kind`: what its values ARE, as opposed
///     to what its driver calls them — `timestamp`, `integer`, `float`,
///     `text`, `bool`, `json`, `binary` or `unknown`. Eighteen drivers have
///     eighteen vocabularies for the same eight ideas, and a client deciding
///     whether a column can go on a time axis should not have to learn
///     `int8`, `BIGINT`, `Int64` and `NUMBER(38,0)`. Derived in ONE table from
///     the type name rather than filled in by each driver, so there is one
///     answer rather than eighteen; a driver that declares no type says
///     `unknown` rather than guessing. Additive — a client that ignores it
///     behaves exactly as before.
/// 19. `store.charts.save` takes `kind` and every chart row carries it: how
///     the chart is DRAWN — bars, line, area, scatter, pie. The words are the
///     client's and the engine neither draws nor interprets them, so a newer
///     client can save a kind this engine never heard of. Additive: a client
///     that sends none keeps the empty string, which means "the client's
///     default".
pub const PROTO: u32 = 19;

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
