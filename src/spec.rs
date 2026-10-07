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
    /// Can report what its server is doing (`instance.metrics`).
    ///
    /// Separate from everything else because what an engine will tell you about
    /// itself varies more than anything: PostgreSQL has a whole statistics
    /// collector, SQLite has a handful of PRAGMAs about a file, and a driver
    /// speaking to someone else's hosted service may be allowed none of it.
    pub metrics: bool,
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
    /// How THIS engine spells "between these two moments", as a fragment.
    ///
    /// For a dashboard's shared time range. `{col}` is the column carrying the
    /// time, `{from}` and `{to}` the ends of the range — and those two have
    /// already been through `dashboard::parse_timestamp`, which refuses
    /// anything that is not exactly a timestamp, so what reaches this template
    /// is a date and nothing else.
    ///
    /// A TEMPLATE, like `browse`, for the same reason: the client holds every
    /// value and a round trip to format a string is a round trip to format a
    /// string. What it must not do is INVENT the words, and this is where the
    /// words come from — MongoDB's is a JSON object and Redis has no such thing
    /// at all, which an empty string says.
    ///
    /// A chart statement marks where this goes with `{{time}}`. One that does
    /// not carry the marker is not time-filtered, and the board says so on that
    /// tile rather than pretending — a range that silently applies to four
    /// charts out of six is worse than one that says which two it missed.
    pub time_filter: &'static str,
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

/// How nearly every SQL dialect spells a time window. Half-open at the top:
/// `BETWEEN` would include the final instant, so two adjacent ranges would both
/// contain the row exactly on the boundary and a total split in two would come
/// out larger than the whole.
pub const SQL_TIME_FILTER: &str = "{col} >= '{from}' AND {col} < '{to}'";

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
    /// The SAVED connection's name, when this spec came from one.
    ///
    /// The engine does not use it to connect — it is here so the audit log can
    /// say WHICH connection a statement ran on, in the word a person would
    /// search for, instead of a conn_id that means nothing once the process
    /// restarts. Additive and optional: a client that does not send it gets a
    /// log that names the driver and leaves the name empty, which is a gap that
    /// reads as a gap rather than a wrong answer.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub auth: AuthSpec,
    #[serde(default)]
    pub tls: TlsSpec,
    #[serde(default)]
    pub tunnel: Option<TunnelSpec>,
    /// A SAVED tunnel, by name, instead of one spelled out here.
    ///
    /// A bastion is one machine and twenty connections go through it; writing
    /// its host, user and key into twenty specs means changing twenty of them
    /// when the key rotates, and finding out which one was missed by its
    /// failing. The engine resolves the name at connect time, so a profile
    /// edited once is edited everywhere.
    ///
    /// Naming a profile AND spelling a tunnel out is refused rather than
    /// resolved by precedence: a connection with two tunnels is a connection
    /// whose author believed something that is not true about it.
    #[serde(default)]
    pub tunnel_profile: Option<String>,
    /// A proxy every TCP dial goes through — the database's, and the SSH
    /// server's.
    #[serde(default)]
    pub proxy: Option<ProxySpec>,
    /// A saved proxy, by name. Same rule as `tunnel_profile`.
    #[serde(default)]
    pub proxy_profile: Option<String>,
    /// The person's own code, run around this connection's lifetime.
    #[serde(default)]
    pub hooks: Vec<Hook>,
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
    /// The two ways a tunnel and a proxy can be ambiguous, refused with the
    /// name of the one that is ambiguous.
    ///
    /// Checked before a profile is looked up, so "you named a profile AND wrote
    /// a tunnel" is the answer rather than "no such profile".
    pub fn check_profiles(&self) -> anyhow::Result<()> {
        if self.tunnel.is_some() && self.tunnel_profile.is_some() {
            anyhow::bail!(
                "this connection both names the tunnel profile '{}' and spells a tunnel out — \
                 pick one, because two tunnels is not a thing a connection can have",
                self.tunnel_profile.as_deref().unwrap_or("")
            );
        }
        if self.proxy.is_some() && self.proxy_profile.is_some() {
            anyhow::bail!(
                "this connection both names the proxy profile '{}' and spells a proxy out — pick one",
                self.proxy_profile.as_deref().unwrap_or("")
            );
        }
        Ok(())
    }

    /// One line naming WHAT this connection points at, for the audit log.
    ///
    /// Built from the fields the engine itself reads generically — the `host`
    /// and `port` params, the `File` param the driver's meta declares, the TLS
    /// mode and the tunnel's hop. Deliberately NOT the whole `params` map: that
    /// map is free-form and driver-interpreted, and a log line assembled from
    /// keys nobody has looked at is how something nobody meant to keep ends up
    /// kept. Deliberately not `auth` either — those fields are `Secret`, and the
    /// honest way to keep a credential out of a log is to never read it.
    pub fn target(&self, meta: &DriverMeta) -> String {
        let mut out = String::from(&self.driver);
        if let Some(fp) = meta.params.iter().find(|p| matches!(p.kind, ParamType::File)) {
            if let Some(path) = self.param_opt(fp.key) {
                out.push(' ');
                out.push_str(path);
            }
        } else if let Some(host) = self.param_opt("host") {
            out.push_str(&format!(" {host}:{}", self.port(meta.default_port.unwrap_or(0))));
        }
        out.push_str(&format!(" tls={}", self.tls.mode.label()));
        if let Some(t) = &self.tunnel {
            out.push_str(&format!(" via ssh {}@{}:{}", t.user, t.host, t.port));
        }
        // Which machine the traffic actually left through belongs in the record
        // as much as which database it reached.
        if let Some(p) = &self.proxy {
            out.push_str(&format!(" through {} {}:{}", p.kind.label(), p.host, p.port));
        }
        out
    }

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

impl TlsMode {
    /// The spelling a person chose in the form — the same word in the log.
    pub fn label(self) -> &'static str {
        match self {
            TlsMode::Disable => "disable",
            TlsMode::Prefer => "prefer",
            TlsMode::Require => "require",
            TlsMode::VerifyCa => "verify_ca",
            TlsMode::VerifyFull => "verify_full",
        }
    }
}

/// Something of the person's own that runs when a connection opens or closes.
///
/// The four moments are the four that mean anything: before the dial, after it
/// succeeded, before the link is taken down, after it is gone. A PreConnect hook
/// that FAILS stops the connection, which is the only reason to have one before
/// rather than after — opening a VPN, warming a credential, refusing to let
/// anybody near production on a Friday.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Hook {
    pub on: HookEvent,
    #[serde(default)]
    pub kind: HookKind,
    /// A command line, a saved script's NAME, or an inline body — whichever the
    /// kind says. A saved script by NAME rather than by position, for the same
    /// reason a dashboard tile names its chart.
    pub source: String,
    /// For an inline hook: which language the body is.
    #[serde(default)]
    pub lang: Option<crate::runner::Lang>,
    /// How long it may take. `None` is the engine's default, not "forever": a
    /// hook with no limit is a connection that never opens and never says why.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreConnect,
    PostConnect,
    PreDisconnect,
    PostDisconnect,
}

impl HookEvent {
    pub fn label(self) -> &'static str {
        match self {
            HookEvent::PreConnect => "pre_connect",
            HookEvent::PostConnect => "post_connect",
            HookEvent::PreDisconnect => "pre_disconnect",
            HookEvent::PostDisconnect => "post_disconnect",
        }
    }

    /// Whether a failure here stops what was about to happen.
    ///
    /// Only PreConnect. The others run after the thing they are named for, so
    /// there is nothing left to stop — and pretending otherwise (refusing to
    /// disconnect because a hook failed) would leave a connection open that
    /// somebody asked to close.
    pub fn blocking(self) -> bool {
        matches!(self, HookEvent::PreConnect)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// `source` is a command line, run through a shell.
    #[default]
    Command,
    /// `source` names a saved script.
    Script,
    /// `source` is the body, and `lang` says what it is written in.
    Inline,
}

/// A proxy every TCP dial goes through: the DB's, and the SSH server's.
///
/// Orthogonal to the tunnel, and UNDERNEATH it. A connection may have both, and
/// then the order is proxy → SSH server → direct-tcpip → database, because that
/// is what the two things mean: a proxy is how this machine reaches anything at
/// all, and a tunnel is what rides on top of a reachable SSH server.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxySpec {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub auth: Option<ProxyAuth>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKind {
    Socks5,
    HttpConnect,
}

impl ProxyKind {
    pub fn label(self) -> &'static str {
        match self {
            ProxyKind::Socks5 => "SOCKS5",
            ProxyKind::HttpConnect => "HTTP CONNECT",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyAuth {
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: Secret,
}

/// An SSH tunnel fronting the DB's TCP endpoint — orthogonal to both auth and
/// TLS, applied by the net layer before the driver dials. Wired with russh in
/// the remote/auth phase.
#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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

/// One thing an engine says about itself.
///
/// Deliberately flat text rather than a number: "4.0 MiB", "wal", "3 of 100
/// connections". A metric that arrives as a bare number needs a unit and a
/// scale from somewhere, and the only place that knows both is the driver that
/// asked for it.
#[derive(Debug, Clone, Serialize)]
pub struct Metric {
    /// Which heading it belongs under — "storage", "activity", "settings".
    pub group: &'static str,
    pub name: &'static str,
    pub value: String,
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
