// lode/spec: the wire data model shared by the RPC layer, the driver
// trait, and every driver impl.
//
// These types are the CONTRACT with the Lua side: a `ConnSpec` is what the
// connection form produces (driver kind + a typed param map + a typed auth
// method + TLS + optional SSH tunnel), and `DriverMeta` is what the Lua side
// reads back from `rpc.hello` to BUILD that form dynamically — so adding a DB
// type never edits any Lua. Secrets live here as templates (see secret.rs);
// they are resolved in the daemon at connect time only, never logged.

// This module is the WIRE CONTRACT: its auth variants, TLS/tunnel fields and
// Value cell types are consumed incrementally as each driver phase lands, so
// some are not yet constructed by the single compiled driver of an early build.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::secret::Secret;

/// A capability bitset a driver advertises so the UI can adapt (label the editor
/// SQL vs command, show the schema tree, offer cancel, allow a tunnel, …).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Caps {
    pub sql: bool,      // statements are SQL (vs commands / JSON)
    pub schemas: bool,  // has a schema → object tree to browse
    pub cancel: bool,   // supports protocol-level query cancellation
    pub tls: bool,      // supports TLS to the server
    pub tunnel: bool,   // a TCP endpoint that an SSH tunnel can front
    pub multi_db: bool, // exposes multiple databases on one connection
    pub kv: bool,       // key/value or document store (not relational SQL)
    // Per-object INTROSPECTION beyond the columns every schema driver already serves. Advertised
    // separately because support is genuinely uneven and NOT implied by `schemas`: a document/KV store has
    // no DDL to show at all, and several SQL engines expose indexes but no server-side "CREATE statement"
    // (there the honest answer is "cannot", not a hand-assembled approximation). The drawer offers a helper
    // ONLY where its driver claims it, so no engine ever grows a row that dead-ends.
    pub indexes: bool, // can list an object's indexes (`schema.indexes`)
    pub ddl: bool,     // can return an object's CREATE statement (`schema.ddl`)
    /// Can change ONE addressed row (`row.write`).
    ///
    /// Claimed only by a driver that can do it SAFELY — that is, one that binds
    /// values as parameters rather than pasting them into a string, and that can
    /// undo a write which turned out to touch more than the row it was aimed at.
    /// A driver which can merely build an UPDATE does NOT claim this: the risk
    /// is not writing the statement, it is being wrong about which rows it hits.
    pub edit: bool,
}

/// The kinds of auth a driver can accept. A driver lists the subset it supports
/// in `DriverMeta.auth`; the connection form then renders only those methods'
/// fields for the chosen driver — no hardcoded field set anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    None,
    Password,
    ClientCert, // TLS mutual / X.509 (also Snowflake key-pair JWT key file)
    Provider,   // IAM / OAuth / OIDC bearer, driver-native
    Kerberos,   // GSSAPI where the driver implements it natively
}

/// One typed field in the connection form (host, port, database, file, url, …).
/// `secret = true` marks a field whose value is a Secret TEMPLATE.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ParamSpec {
    pub key: &'static str,
    pub label: &'static str,
    #[serde(rename = "type")]
    pub kind: ParamType,
    pub required: bool,
    pub secret: bool,
    pub default: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    String,
    Int,
    Bool,
    File,
    Enum,
}

/// Static, driver-declared metadata. Serialized to Lua verbatim by `rpc.hello`,
/// so the Lua side lists driver kinds and builds each connection form from it.
#[derive(Debug, Clone, Serialize)]
pub struct DriverMeta {
    pub kind: &'static str,
    pub display: &'static str,
    pub default_port: Option<u16>,
    pub params: &'static [ParamSpec],
    pub auth: &'static [AuthKind],
    pub caps: Caps,
    /// What an empty editor should suggest — IN THIS ENGINE'S LANGUAGE.
    ///
    /// A client showed `SELECT …` for every driver, so a MongoDB connection
    /// invited SQL and answered `statement must be a JSON document: expected
    /// value at line 1 column 1`. The engine is the only side that knows what
    /// each driver speaks, and a client that knew would be a list of driver
    /// names in a client — the thing `params` and `auth` exist to avoid.
    pub hint: &'static str,
    /// How to read the first rows of ONE object, as a template.
    ///
    /// The client substitutes `{qualified}` (the schema-qualified name, or the
    /// bare name where there is no schema), `{name}`, `{schema}` and `{limit}`.
    /// A template rather than an RPC: the client already holds every value, and
    /// a round trip to format a string is a round trip to format a string.
    pub browse: &'static str,
    /// Overrides of `browse` for particular node kinds, as `schema.structure`
    /// labels them.
    ///
    /// Most engines need none: a table is a table. Redis needs five, because
    /// reading a key depends on what the key IS — a string is `GET`, a hash is
    /// `HGETALL`, a sorted set is `ZRANGE`. Expressing that as a map keyed by
    /// the node kind keeps the knowledge here, where the driver already decides
    /// what to call each node, instead of in a client matching on driver names.
    pub browse_by_kind: &'static [(&'static str, &'static str)],
    /// The fields of a structured query bar. EMPTY means this engine is typed
    /// as free text in an editor, which is what every SQL driver wants.
    pub query_form: &'static [QueryFieldSpec],
    /// The statement those fields are assembled into. `{name}`, `{qualified}`,
    /// `{schema}` and `{limit}` are substituted as in `browse`; `{fragments}`
    /// becomes the non-empty fields' fragments joined with `", "`, prefixed
    /// with `", "` when there is at least one and empty when there is none — so
    /// a skeleton can be written as valid JSON with no fields filled in.
    pub query_template: &'static str,
    /// Fields that narrow a BROWSE — a WHERE and an ORDER BY, for the engines
    /// that have them.
    ///
    /// Separate from `query_form` on purpose. `query_form` REPLACES the free
    /// statement box, which is right for MongoDB and wrong for SQL: taking away
    /// the box to offer two inputs would be a loss. These fields sit beside the
    /// browse a click produced and feed `{clauses}` in the template, so the
    /// driver still owns every word of SQL — the client contributes a value, not
    /// a syntax.
    pub browse_form: &'static [QueryFieldSpec],
    /// The comparisons this dialect offers in the visual builder.
    ///
    /// A label and the fragment it produces, with `{col}` and `{value}` filled
    /// in. Declared per driver rather than assumed, because the list is NOT the
    /// same everywhere: `ILIKE` is PostgreSQL's, `IS NULL` is spelled the same
    /// in all of them and `<>` is not universally preferred over `!=`. Empty
    /// means this engine has no builder, which is the right answer for a
    /// key/value store.
    pub builder_ops: &'static [BuilderOp],
}

/// One comparison the builder can offer.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct BuilderOp {
    /// What the person picks from the list.
    pub label: &'static str,
    /// The condition it becomes. `{col}` is the column, `{value}` the typed
    /// text. An op with no `{value}` (IS NULL) takes no input, and the builder
    /// hides the box rather than showing one that is ignored.
    pub fragment: &'static str,
}

/// One input of a STRUCTURED query bar, for engines whose statements are not a
/// language a person wants to type by hand.
///
/// MongoDB is the case that forced it: "write a command document" is a true
/// instruction and a bad interface, and every other client of it shows filter /
/// projection / sort / limit as four fields. Which fields exist, what they are
/// called and how each one contributes to the statement are all facts about the
/// driver, so they live here rather than in a client matching on driver names.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct QueryFieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    /// Shown in the empty input — a real example, not a restatement of the label.
    pub placeholder: &'static str,
    /// What this field adds to the statement when it is NOT empty. `{value}` is
    /// substituted with what was typed.
    pub fragment: &'static str,
    /// What the field starts as.
    pub default: &'static str,
    /// How many lines the input gets. 1 is a single-line field.
    pub rows: u8,
}

/// The comparisons nearly every SQL dialect spells the same way. A driver with
/// more of its own starts from this and adds to it.
pub const SQL_BUILDER_OPS: &[BuilderOp] = &[
    BuilderOp { label: "=", fragment: "{col} = {value}" },
    BuilderOp { label: "<>", fragment: "{col} <> {value}" },
    BuilderOp { label: "<", fragment: "{col} < {value}" },
    BuilderOp { label: "<=", fragment: "{col} <= {value}" },
    BuilderOp { label: ">", fragment: "{col} > {value}" },
    BuilderOp { label: ">=", fragment: "{col} >= {value}" },
    BuilderOp { label: "LIKE", fragment: "{col} LIKE {value}" },
    BuilderOp { label: "IN", fragment: "{col} IN ({value})" },
    BuilderOp { label: "IS NULL", fragment: "{col} IS NULL" },
    BuilderOp { label: "IS NOT NULL", fragment: "{col} IS NOT NULL" },
];

/// The clauses every SQL driver offers on a browse. Identical across them
/// because this much of SQL genuinely is: a `WHERE` and an `ORDER BY` after the
/// table and before the limit.
pub const SQL_BROWSE_FORM: &[QueryFieldSpec] = &[
    QueryFieldSpec {
        key: "filter",
        label: "Where",
        placeholder: "balance < 0",
        fragment: "WHERE {value}",
        default: "",
        rows: 1,
    },
    QueryFieldSpec {
        key: "order",
        label: "Order by",
        placeholder: "balance DESC",
        fragment: "ORDER BY {value}",
        default: "",
        rows: 1,
    },
];

impl DriverMeta {
    /// The browse template for an object of this `kind`.
    pub fn browse_for(&self, kind: &str) -> &'static str {
        self.browse_by_kind
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, t)| *t)
            .unwrap_or(self.browse)
    }
}

// ── the incoming connection spec (built by the Lua form) ─────────────────────

/// Everything needed to open one connection. `params` is a free-form string map
/// keyed by the driver's ParamSpec keys (the daemon interprets them per driver);
/// `auth`, `tls`, `tunnel` are typed.
#[derive(Debug, Clone, Deserialize)]
pub struct ConnSpec {
    pub driver: String,
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub auth: AuthSpec,
    #[serde(default)]
    pub tls: TlsSpec,
    #[serde(default)]
    pub tunnel: Option<TunnelSpec>,
    /// What this connection is ALLOWED to do, enforced by the engine.
    ///
    /// On the connection rather than on a client, because a client is a thing
    /// that can be rewritten, replaced or simply forget — and "the production
    /// database is read-only from here" has to survive all three.
    #[serde(default)]
    pub policy: Policy,
}

/// What a connection may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Nothing that writes. Not an UPDATE, not an INSERT, not a CREATE — the
    /// line is "does this change the database", not "does this look dangerous",
    /// because an INSERT into the wrong table is not dangerous-looking and is
    /// still a change somebody did not want made.
    ReadOnly,
    /// Writes are allowed; the ones `classify` names have to be confirmed. This
    /// is the default: it is the behaviour somebody expects without having
    /// configured anything, and it fails toward asking.
    #[default]
    Confirm,
    /// Everything runs. For a scratch database where the asking is the nuisance
    /// rather than the write.
    Open,
}

impl Policy {
    pub fn label(self) -> &'static str {
        match self {
            Policy::ReadOnly => "read only",
            Policy::Confirm => "asks first",
            Policy::Open => "unrestricted",
        }
    }
}

impl ConnSpec {
    /// A required string param, or an error naming the missing key.
    pub fn param(&self, key: &str) -> anyhow::Result<&str> {
        self.params
            .get(key)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter '{key}'"))
    }

    /// An optional string param.
    pub fn param_opt(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str).filter(|s| !s.is_empty())
    }

    /// An optional param parsed as a port, falling back to `default`.
    pub fn port(&self, default: u16) -> u16 {
        self.param_opt("port").and_then(|s| s.parse().ok()).unwrap_or(default)
    }
}

/// A typed auth method. `password` and the various key/token fields carry Secret
/// TEMPLATES (`{{ env "VAR" }}` / `{{ cmd "…" }}` / literal), resolved only at
/// connect time and never logged.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthSpec {
    #[default]
    None,
    Password {
        #[serde(default)]
        user: String,
        #[serde(default)]
        password: Secret,
    },
    ClientCert {
        cert: String,
        key: String,
        #[serde(default)]
        key_password: Secret,
        #[serde(default)]
        user: String,
    },
    Provider {
        /// e.g. "aws" | "oauth" | "oidc" — interpreted per driver.
        provider: String,
        #[serde(default)]
        token: Secret,
        #[serde(default)]
        user: String,
    },
    Kerberos {
        #[serde(default)]
        principal: Option<String>,
    },
}

/// TLS to the server — orthogonal to auth, negotiated with rustls (no OpenSSL).
/// The SAFE default is `Prefer`: encryption is attempted and used whenever the
/// server supports it, and a link that ends up unencrypted is surfaced (never
/// silently accepted). `ca` verifies the server; `client_cert`/`client_key`
/// enable mutual X.509 (client-certificate authentication).
#[derive(Debug, Clone, Deserialize)]
pub struct TlsSpec {
    #[serde(default)]
    pub mode: TlsMode,
    #[serde(default)]
    pub ca: Option<String>,
    #[serde(default)]
    pub client_cert: Option<String>,
    #[serde(default)]
    pub client_key: Option<String>,
}

impl Default for TlsSpec {
    fn default() -> Self {
        Self {
            mode: TlsMode::Prefer,
            ca: None,
            client_cert: None,
            client_key: None,
        }
    }
}

impl TlsSpec {
    /// Whether TLS must be negotiated (a plaintext-only server is rejected).
    pub fn required(&self) -> bool {
        matches!(self.mode, TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull)
    }

    /// Whether TLS should be attempted at all.
    pub fn wanted(&self) -> bool {
        !matches!(self.mode, TlsMode::Disable)
    }

    /// Whether the server certificate chain is verified against `ca` / system roots.
    pub fn verifies_cert(&self) -> bool {
        matches!(self.mode, TlsMode::VerifyCa | TlsMode::VerifyFull)
    }

    /// Whether the server hostname is verified (VerifyFull only).
    pub fn verifies_hostname(&self) -> bool {
        matches!(self.mode, TlsMode::VerifyFull)
    }
}

/// TLS posture, from least to most strict. The default is `Prefer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Explicit opt-out: no encryption attempted (the only unencrypted path).
    Disable,
    /// Try TLS; use it when the server supports it, else plaintext (surfaced).
    #[default]
    Prefer,
    /// Encryption REQUIRED, but the server certificate is not verified.
    Require,
    /// Encryption required + the certificate chain is verified against the CA.
    VerifyCa,
    /// Encryption required + chain AND hostname verified (the strictest).
    VerifyFull,
}

/// An SSH tunnel fronting the DB's TCP endpoint — orthogonal to both auth and
/// TLS, applied by the net layer before the driver dials. Wired with russh in
/// the remote/auth phase.
#[derive(Debug, Clone, Deserialize)]
pub struct TunnelSpec {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    pub auth: TunnelAuth,
}

fn default_ssh_port() -> u16 {
    22
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TunnelAuth {
    Agent,
    Key {
        path: String,
        #[serde(default)]
        passphrase: Secret,
    },
    Password {
        #[serde(default)]
        password: Secret,
    },
}

// ── result model (uniform across every driver) ───────────────────────────────

/// One result column.
#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
}

/// One column of a browsable OBJECT — what the drawer's `Columns` facet lists.
///
/// Deliberately NOT the same type as `Column` above, which is a RESULT column: a query result has no primary
/// key (its rows may come from a join, an aggregate, an expression), so carrying `primary` there would put a
/// permanently-false field on every result set and invite someone to trust it. They are two different
/// questions — "what did this query return" and "how is this object defined" — and only the second has an
/// answer here.
///
/// `primary` is load-bearing, not decoration: it is what lets the result grid address ONE row for an edit.
/// It must come from the COLUMNS and not from the primary INDEX, because the two disagree — sqlite makes no
/// index at all for `id INTEGER PRIMARY KEY` (that column is the rowid alias), so an index-derived key calls
/// an obviously-keyed table unaddressable.
#[derive(Debug, Clone, Serialize)]
pub struct TableColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    /// Part of the object's identifying key (a PK column; mongo's `_id`; a CQL partition/clustering key).
    pub primary: bool,
}

/// Read a value that came in over the wire, back into the enum the drivers bind.
///
/// The tagged forms are read back deliberately: a client that received an
/// `{"__oid": …}` and sent it back unchanged must address the SAME ObjectId,
/// not a 24-character string that matches no document. This is the other half of
/// the round trip `Value`'s doc comment describes — without it the type survives
/// the way OUT and is lost on the way back, which is the same bug one direction
/// later.
pub fn value_from_json(v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        J::String(s) => Value::Text(s.clone()),
        J::Object(m) => {
            if let Some(J::String(hex)) = m.get("__oid") {
                return Value::Oid(hex.clone());
            }
            if let (Some(J::String(key)), Some(J::String(text))) = (m.get("__ext"), m.get("v")) {
                return Value::Ext { text: text.clone(), key: key.clone() };
            }
            if let (Some(J::String(b64)), Some(len)) = (m.get("b64"), m.get("len")) {
                return Value::Bytes {
                    b64: b64.clone(),
                    len: len.as_u64().unwrap_or(0) as usize,
                };
            }
            Value::Json(v.clone())
        }
        J::Array(_) => Value::Json(v.clone()),
    }
}

/// A change to ONE row, named by its identifying key.
///
/// The client sends values, never SQL. The driver builds the statement in its
/// own dialect and binds every value as a parameter, which is why there is no
/// quoting anywhere in the client and no way for a value to become syntax.
#[derive(Debug, Clone, Deserialize)]
pub struct RowPlan {
    pub obj: ObjRef,
    /// The identifying columns and their values. The engine checks this covers
    /// EXACTLY the object's key — not a subset, not a guess from the other
    /// columns' values.
    pub key: Vec<(String, serde_json::Value)>,
    /// The columns to change. Empty with `delete` false is nothing to do.
    #[serde(default)]
    pub set: Vec<(String, serde_json::Value)>,
    /// Remove the row instead of changing it.
    #[serde(default)]
    pub delete: bool,
}

/// What a row write did.
#[derive(Debug, Clone, Serialize)]
pub struct RowOutcome {
    /// The statement the driver built, with its placeholders — shown to the
    /// person, never re-parsed by anyone.
    pub statement: String,
    pub affected: u64,
}

/// A schema-tree node (schema → table/view/collection). `kind` drives the icon.
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub name: String,
    pub kind: String, // "schema" | "table" | "view" | "collection" | "column" | …
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
}

/// One index on a browsable object. Deliberately a SMALL common shape: every engine that has indexes has a
/// name, the columns it covers, and the unique/primary distinction — everything past that (method, partial
/// predicates, collation, storage options) is per-engine and belongs in the DDL, not in a lowest-common
/// struct that would be mostly `None`.
#[derive(Debug, Clone, Serialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

/// A reference to a browsable object (for `schema.columns` / `schema.indexes` / `schema.ddl`).
#[derive(Debug, Clone, Deserialize)]
pub struct ObjRef {
    pub name: String,
    #[serde(default)]
    pub schema: Option<String>,
}

/// A single result cell. Serializes to a plain JSON value the Lua grid can
/// render directly: scalars as scalars, bytes/timestamp as a tagged object so
/// the grid can show a marker and fetch the full value via `query.cell`.
///
/// A variant exists whenever the grid must be able to tell apart two values that
/// would RENDER the same. That is not a display concern — it is what decides
/// whether a row can be written back at all. `Oid` is the case that proved it:
/// rendering a MongoDB ObjectId to its hex as `Text` made it indistinguishable
/// from a string `_id`, so the grid could only ever match `{_id: "<hex>"}` —
/// which matches NOTHING against a real ObjectId, i.e. an update that reports
/// success and changes no document. The type has to survive the trip: Lua can
/// only judge what IT rendered, never recover what this enum already discarded.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes {
        b64: String,
        len: usize,
    },
    Json(serde_json::Value),
    Timestamp(String),
    /// A MongoDB ObjectId, as its 24-char hex. Tagged (`{ __oid }`) rather than
    /// plain text so the grid can render the hex yet still address the document
    /// with the `{"$oid": …}` extended-JSON form the server matches on.
    Oid(String),
    /// A value that DISPLAYS as `text` but, on a write-back, round-trips through the MongoDB extended-JSON
    /// key `key` (e.g. `$date`, `$numberDecimal`): a top-level DateTime / Decimal128 shows its readable form
    /// yet an edit is re-emitted as `{key: <text>}` so `Bson::try_from` reconstructs the real BSON type.
    /// This generalises the `Oid` tag to the other types that would otherwise render as an untagged string
    /// and be written back as the wrong type. Serializes as `{ "__ext": key, "v": text }`.
    Ext {
        text: String,
        key: String,
    },
}

/// Expand a leading `~` / `~/` to $HOME (a file path a driver will open). Shared so the form's Test path
/// and the drivers' `connect` agree — otherwise Test reports a `~/db.sqlite` readable while connect fails.
pub fn expand_tilde(path: &str) -> String {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => match std::env::var("HOME") {
            Ok(home) => format!("{home}{rest}"),
            Err(_) => path.to_string(),
        },
        _ => path.to_string(),
    }
}

impl Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Value::Null => s.serialize_none(),
            Value::Bool(b) => s.serialize_bool(*b),
            Value::Int(i) => {
                // A JSON number becomes a LuaJIT double on the client, losing precision above 2^53 (and
                // %.14g even rounds 15-16 digit ints). Send a plain number only when it round-trips exactly;
                // otherwise tag it as decimal TEXT (`{ "__int": "<digits>" }`) so the grid display and the
                // generated UPDATE keep the exact value — mirrors the `__oid`/`__ext` tagging precedent.
                if i.unsigned_abs() < (1u64 << 53) {
                    s.serialize_i64(*i)
                } else {
                    let mut m = s.serialize_map(Some(1))?;
                    m.serialize_entry("__int", &i.to_string())?;
                    m.end()
                }
            }
            Value::Float(f) => s.serialize_f64(*f),
            Value::Text(t) => s.serialize_str(t),
            Value::Timestamp(t) => s.serialize_str(t),
            Value::Json(j) => j.serialize(s),
            Value::Bytes { b64, len } => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("__bytes", b64)?;
                m.serialize_entry("len", len)?;
                m.end()
            }
            Value::Oid(hex) => {
                let mut m = s.serialize_map(Some(1))?;
                m.serialize_entry("__oid", hex)?;
                m.end()
            }
            Value::Ext { text, key } => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("__ext", key)?;
                m.serialize_entry("v", text)?;
                m.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_serializes_as_plain_json() {
        assert_eq!(serde_json::to_string(&Value::Null).unwrap(), "null");
        assert_eq!(serde_json::to_string(&Value::Int(7)).unwrap(), "7");
        assert_eq!(serde_json::to_string(&Value::Text("hi".into())).unwrap(), "\"hi\"");
        let b = Value::Bytes {
            b64: "AA==".into(),
            len: 1,
        };
        assert_eq!(serde_json::to_string(&b).unwrap(), "{\"__bytes\":\"AA==\",\"len\":1}");
    }

    #[test]
    fn connspec_deserializes_auth_variants() {
        let j = r#"{"driver":"postgres","params":{"host":"h","port":"5432","database":"d"},
                    "auth":{"kind":"password","user":"u","password":"p"}}"#;
        let spec: ConnSpec = serde_json::from_str(j).unwrap();
        assert_eq!(spec.driver, "postgres");
        assert_eq!(spec.param("host").unwrap(), "h");
        assert_eq!(spec.port(5432), 5432);
        assert!(matches!(spec.auth, AuthSpec::Password { .. }));
    }
}
