// lode/mcp: the engine, offered to something that is not a person.
//
// `loded --mcp` speaks the Model Context Protocol on stdio instead of lode's
// own. It is the SAME `Server` underneath: the same drivers, the same store,
// the same `Driver::classify`, the same connection policy, the same audit log.
// There is no second, softer path through any of it, because a second path is
// the one that gets used.
//
// What is DIFFERENT is only ever more restrictive, and there are three things:
//
//   1. A ROLE. `reader` by default, and a reader refuses every write whatever
//      the connection allows. The role is a floor, not a key.
//   2. `Policy::Open` is read as `Confirm`. "Unrestricted" is a decision a
//      person made about THEMSELVES — "stop asking me" — and it is not
//      inherited by a machine acting in their name.
//   3. The agent CANNOT CONFIRM. When `classify` names a statement, the tool
//      does not run it: it files an approval for a person and says so. The
//      person answers in their own client, and the next attempt spends that
//      approval — once, for that exact statement.
//
// The approval queue is in the STORE rather than in memory, because the two
// sides are different processes: this one, and whatever window the person has
// open. A queue in memory would be a queue only the asker can see.
//
// Every tool call is audited. An agent reading a database is a thing that
// happened, and the record of what happened does not get to leave out the
// calls that were not made by a person.

use serde_json::{json, Value as Json};

use crate::server::{AgentRole, Server};

/// The protocol revision this speaks. Answered verbatim in `initialize`.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// One tool, as `tools/list` describes it.
struct Tool {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Json,
}

fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "list_connections",
            description: "The databases this engine has saved, by name. Returns names and driver \
                          kinds only — never a host, never a credential.",
            schema: || json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: "open",
            description: "Open a saved connection by name and return a connection id to use with \
                          the other tools. Fails if the connection does not exist; it will not \
                          invent one.",
            schema: || {
                json!({
                    "type": "object",
                    "required": ["name"],
                    "properties": { "name": { "type": "string" } }
                })
            },
        },
        Tool {
            name: "list_objects",
            description: "The tables, views and collections on an open connection, as the driver \
                          itself describes them.",
            schema: || {
                json!({
                    "type": "object",
                    "required": ["conn_id"],
                    "properties": { "conn_id": { "type": "integer" } }
                })
            },
        },
        Tool {
            name: "describe",
            description: "The columns, the identifying key and the indexes of one object.",
            schema: || {
                json!({
                    "type": "object",
                    "required": ["conn_id", "name"],
                    "properties": {
                        "conn_id": { "type": "integer" },
                        "name": { "type": "string" },
                        "schema": { "type": "string" }
                    }
                })
            },
        },
        Tool {
            name: "query",
            description: "Run one statement and return its first rows. The statement is held \
                          against the connection's policy and this session's role: a reader \
                          session cannot change anything, and a statement the engine calls \
                          dangerous is NOT run — it is filed for a person to approve, and this \
                          returns what was filed.",
            schema: || {
                json!({
                    "type": "object",
                    "required": ["conn_id", "statement"],
                    "properties": {
                        "conn_id": { "type": "integer" },
                        "statement": { "type": "string" },
                        "limit": { "type": "integer", "description": "rows to return, at most 200" }
                    }
                })
            },
        },
        Tool {
            name: "session",
            description: "What this session is allowed to do: its role, and what that means.",
            schema: || json!({ "type": "object", "properties": {} }),
        },
    ]
}

/// How many rows a tool will ever return.
///
/// A ceiling rather than a default, because the caller is a language model and
/// a million-row answer is a million rows of context nobody chose to spend.
const MAX_ROWS: usize = 200;

pub struct Mcp {
    server: Server,
    role: AgentRole,
}

impl Mcp {
    pub fn new(server: Server, role: AgentRole) -> Mcp {
        Mcp { server, role }
    }

    /// Handle one MCP request and produce its reply, or `None` for a
    /// notification (which, per JSON-RPC, is answered with nothing at all).
    pub async fn handle(&self, line: &str) -> Option<String> {
        let req: Json = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(err_reply(Json::Null, -32700, &format!("not JSON: {e}"))),
        };
        let id = req.get("id").cloned().unwrap_or(Json::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        // A notification has no id and gets no answer. `notifications/initialized`
        // is the one that matters; answering it would be a protocol error.
        if req.get("id").is_none() {
            return None;
        }

        match method.as_str() {
            "initialize" => Some(ok_reply(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "lode", "version": env!("CARGO_PKG_VERSION") },
                    // Said in the handshake, not buried in a tool: whoever is
                    // driving this should know what it can do before it tries.
                    "instructions": self.instructions(),
                }),
            )),
            "ping" => Some(ok_reply(id, json!({}))),
            "tools/list" => {
                let list: Vec<Json> = tools()
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": (t.schema)(),
                        })
                    })
                    .collect();
                Some(ok_reply(id, json!({ "tools": list })))
            }
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let out = self.call(&name, args).await;
                // A tool that refused is NOT a protocol error. `isError` is how
                // MCP says "the tool ran and said no", and a transport error
                // would make the model think the server is broken rather than
                // that it was told no.
                Some(match out {
                    Ok(text) => ok_reply(id, json!({ "content": [{ "type": "text", "text": text }] })),
                    Err(e) => ok_reply(
                        id,
                        json!({ "content": [{ "type": "text", "text": e.to_string() }], "isError": true }),
                    ),
                })
            }
            other => Some(err_reply(id, -32601, &format!("no such method '{other}'"))),
        }
    }

    fn instructions(&self) -> String {
        format!(
            "This is lode, a database engine. This session's role is '{}'.\n\n\
             Every statement is held against the connection's own policy AND this role, \
             whichever is stricter. A 'reader' session cannot change anything at all. \
             A statement the engine calls dangerous is never run on your say-so: it is \
             filed for a person to approve, and you are told that it was. Asking again \
             before they answer files nothing new.\n\n\
             Nothing here will guess. If a table has no identifying key it is not edited; \
             if a connection does not exist it is not invented.",
            self.role.label()
        )
    }

    async fn call(&self, name: &str, args: Json) -> anyhow::Result<String> {
        match name {
            "session" => Ok(format!(
                "role: {}\n{}",
                self.role.label(),
                match self.role {
                    AgentRole::Reader =>
                        "This session cannot change any database, whatever a connection allows.",
                    AgentRole::Writer =>
                        "This session may write where the connection allows it. Anything the \
                         engine calls dangerous still needs a person to approve it.",
                }
            )),
            "list_connections" => {
                let v = self.server.dispatch_public("store.connections.list", json!({})).await?;
                let rows = v.get("connections").and_then(|c| c.as_array()).cloned().unwrap_or_default();
                if rows.is_empty() {
                    return Ok("There are no saved connections.".into());
                }
                // Names and drivers. NOT the spec: it holds hosts, tunnels and
                // credential templates, and none of that is the agent's.
                let mut out = String::from("name\tdriver\n");
                for r in rows {
                    out.push_str(&format!(
                        "{}\t{}\n",
                        r.get("name").and_then(|x| x.as_str()).unwrap_or(""),
                        r.get("driver").and_then(|x| x.as_str()).unwrap_or("")
                    ));
                }
                Ok(out)
            }
            "open" => {
                let want = args.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let v = self
                    .server
                    .dispatch_public("store.connections.get", json!({ "name": want }))
                    .await?;
                let c = v.get("connection").cloned().unwrap_or(Json::Null);
                if c.is_null() {
                    anyhow::bail!("there is no saved connection called '{want}'");
                }
                let driver = c.get("driver").and_then(|d| d.as_str()).unwrap_or("").to_string();
                let mut spec = c.get("spec").cloned().unwrap_or_else(|| json!({}));
                spec["driver"] = json!(driver);
                spec["name"] = json!(want);
                let v = self.server.dispatch_public("conn.connect", spec).await?;
                let id = v.get("conn_id").and_then(|x| x.as_u64()).unwrap_or(0);
                Ok(format!("opened '{want}' as conn_id {id}"))
            }
            "list_objects" => {
                let conn_id = args.get("conn_id").and_then(|x| x.as_u64()).unwrap_or(0);
                let v = self
                    .server
                    .dispatch_public("schema.structure", json!({ "conn_id": conn_id }))
                    .await?;
                Ok(serde_json::to_string_pretty(&v.get("nodes").cloned().unwrap_or(json!([])))?)
            }
            "describe" => {
                let conn_id = args.get("conn_id").and_then(|x| x.as_u64()).unwrap_or(0);
                let obj = json!({
                    "name": args.get("name").and_then(|x| x.as_str()).unwrap_or(""),
                    "schema": args.get("schema").and_then(|x| x.as_str()),
                });
                let cols = self
                    .server
                    .dispatch_public("schema.columns", json!({ "conn_id": conn_id, "object": obj }))
                    .await?;
                let idx = self
                    .server
                    .dispatch_public("schema.indexes", json!({ "conn_id": conn_id, "object": obj }))
                    .await
                    .unwrap_or_else(|_| json!({}));
                Ok(serde_json::to_string_pretty(&json!({
                    "columns": cols.get("columns").cloned().unwrap_or(json!([])),
                    "indexes": idx.get("indexes").cloned().unwrap_or(json!([])),
                }))?)
            }
            "query" => self.query(args).await,
            other => anyhow::bail!("no such tool '{other}'"),
        }
    }

    /// The one tool that can change something, and therefore the one that
    /// carries all three restrictions.
    async fn query(&self, args: Json) -> anyhow::Result<String> {
        let conn_id = args.get("conn_id").and_then(|x| x.as_u64()).unwrap_or(0);
        let statement = args
            .get("statement")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow::anyhow!("a query needs a statement"))?
            .to_string();
        let limit = args
            .get("limit")
            .and_then(|x| x.as_u64())
            .unwrap_or(MAX_ROWS as u64)
            .min(MAX_ROWS as u64) as usize;

        let out = self
            .server
            .agent_query(conn_id, &statement, self.role, limit)
            .await?;
        Ok(out)
    }
}

fn ok_reply(id: Json, result: Json) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn err_reply(id: Json, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server whose store is a throwaway.
    ///
    /// `Server::new` opens the store, so constructing one in a test opens
    /// SOMETHING — and `data_dir` refuses to pick the real one under test (see
    /// store.rs). This sets the variable as well, so the intent is visible here
    /// and not only in the guard.
    fn test_server() -> Server {
        let dir = std::env::temp_dir().join(format!("lode-mcp-test-{}", std::process::id()));
        std::env::set_var("LODE_DATA_DIR", &dir);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // The receiver is dropped, so sends fail silently — which is what these
        // tests want: they are about the replies, not the notifications.
        Server::new(tx)
    }

    /// Everything a model is told about a tool, checked for the two things that
    /// matter: that the dangerous one SAYS it will not run a dangerous
    /// statement, and that no tool offers to hand over a connection spec.
    #[test]
    fn the_tools_describe_what_they_will_refuse() {
        let all = tools();
        let q = all.iter().find(|t| t.name == "query").unwrap();
        assert!(q.description.contains("is NOT run"), "{}", q.description);
        assert!(q.description.contains("approve"), "{}", q.description);
        let c = all.iter().find(|t| t.name == "list_connections").unwrap();
        assert!(c.description.contains("never a credential"), "{}", c.description);
    }

    #[test]
    fn every_tool_has_a_schema_that_names_what_it_needs() {
        for t in tools() {
            let s = (t.schema)();
            assert_eq!(s["type"], "object", "{}", t.name);
            // A tool with required arguments must list them, or a model will
            // call it with none and get an error instead of a prompt.
            if matches!(t.name, "open" | "list_objects" | "describe" | "query") {
                assert!(s.get("required").is_some(), "{} has no required list", t.name);
            }
        }
    }

    /// A notification gets NO reply. Answering `notifications/initialized` is a
    /// protocol error, and the client that receives it is entitled to complain.
    #[tokio::test]
    async fn a_notification_is_not_answered() {
        let m = Mcp::new(test_server(), AgentRole::Reader);
        assert!(m
            .handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await
            .is_none());
        assert!(m.handle(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).await.is_some());
    }

    /// The handshake says the role out loud. Whoever is driving should know
    /// what it can do before it tries something and is told no.
    #[tokio::test]
    async fn the_handshake_states_the_role() {
        let m = Mcp::new(test_server(), AgentRole::Reader);
        let out = m
            .handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await
            .unwrap();
        assert!(out.contains("2024-11-05"), "{out}");
        assert!(out.contains("role is 'reader'"), "{out}");
        assert!(out.contains("filed for a person to approve"), "{out}");
    }

    /// A tool that refuses is not a transport failure. `isError` is how MCP
    /// says "it ran and said no"; a JSON-RPC error would make the model think
    /// the server is broken rather than that it was told no.
    #[tokio::test]
    async fn a_refused_tool_is_a_result_not_a_protocol_error() {
        let m = Mcp::new(test_server(), AgentRole::Reader);
        let out = m
            .handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#)
            .await
            .unwrap();
        let v: Json = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").is_none(), "{out}");
        assert_eq!(v["result"]["isError"], true, "{out}");
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("nope"));
    }

    #[tokio::test]
    async fn an_unknown_method_is_a_protocol_error() {
        let m = Mcp::new(test_server(), AgentRole::Reader);
        let out = m.handle(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#).await.unwrap();
        let v: Json = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["code"], -32601, "{out}");
    }

    /// The default is the one that cannot do harm. A typo in the variable must
    /// not quietly hand an agent a pen.
    #[test]
    fn an_unrecognised_role_is_a_reader() {
        std::env::set_var("LODE_MCP_ROLE", "administrator");
        assert_eq!(AgentRole::from_env(), AgentRole::Reader);
        std::env::set_var("LODE_MCP_ROLE", "writer");
        assert_eq!(AgentRole::from_env(), AgentRole::Writer);
        std::env::remove_var("LODE_MCP_ROLE");
        assert_eq!(AgentRole::from_env(), AgentRole::Reader);
    }
}
