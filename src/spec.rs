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

use serde::{Deserialize, Serialize, Serializer};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    String,
    Int,
    Bool,
    File,
    Enum,
    /// The name of a profile in `~/.aws/config`.
    ///
    /// A TYPE rather than a client recognising the key `aws_profile`, because a
    /// client that matches on a parameter's name is a client that has to be
    /// edited whenever a driver names something differently — and the next
    /// driver to want this would have to be guessed at. The driver declares what
    /// the field IS; the client decides how to draw it.
    ///
    /// Additive: `access.list` already reports what is in those files, so this
    /// needs no new protocol call. A client that does not know this variant
    /// draws the field as it draws any other string, which is what it did
    /// before.
    AwsProfile,
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
    /// The schema changes this dialect can be asked to write out, as
    /// templates. Empty where the idea does not apply — a key/value store has
    /// no columns to add.
    pub ddl_ops: &'static [DdlOp],
    /// The live views this engine can be ASKED about: who is connected, what
    /// is locked, what is running.
    ///
    /// Declared per driver because the catalogues have nothing in common —
    /// `pg_stat_activity`, `SHOW PROCESSLIST`, `currentOp` and `CLIENT LIST`
    /// are four shapes with four names — and because a client must not guess
    /// at them. Empty means this engine has nothing to show, which is the
    /// honest answer for SQLite and a file in S3.
    pub inspectors: &'static [Inspector],
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

/// One schema change this dialect can be asked to WRITE OUT.
///
/// A template, exactly like `browse` and `time_filter`, and for the same
/// reason: the client holds every value, and a round trip to format a string
/// is a round trip to format a string. What a client must not do is INVENT the
/// words — `ALTER TABLE … RENAME COLUMN` is standard, MySQL before 8 wanted
/// `CHANGE`, and SQL Server wants `sp_rename` with a quoted triple — so this
/// is where the words come from.
///
/// It is GENERATED, never run. The statement goes into the editor where it can
/// be read, changed and then run like anything else, which is the lvim-db
/// model: a schema change that happens because somebody picked a menu item is
/// a schema change nobody reviewed.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DdlOp {
    /// What it does, as a stable word: `add_column`, `rename_column`,
    /// `drop_column`, `create_index`, `drop_index`.
    pub op: &'static str,
    /// What a person reads in the menu.
    pub label: &'static str,
    /// The statement, with `{qualified}`, `{table}`, `{column}`, `{type}`,
    /// `{to}`, `{index}` and `{columns}` filled in by the client.
    pub template: &'static str,
}

/// What nearly every SQL dialect spells the same way.
///
/// A driver with a different spelling starts from this and replaces the one
/// line that differs, rather than writing out five.
pub const SQL_DDL_OPS: &[DdlOp] = &[
    DdlOp {
        op: "add_column",
        label: "Add a column\u{2026}",
        template: "ALTER TABLE {qualified} ADD COLUMN {column} {type};",
    },
    DdlOp {
        op: "rename_column",
        label: "Rename this column\u{2026}",
        template: "ALTER TABLE {qualified} RENAME COLUMN {column} TO {to};",
    },
    DdlOp {
        op: "drop_column",
        label: "Drop this column",
        template: "ALTER TABLE {qualified} DROP COLUMN {column};",
    },
    DdlOp {
        op: "create_index",
        label: "Index this column\u{2026}",
        template: "CREATE INDEX {index} ON {qualified} ({columns});",
    },
    DdlOp { op: "drop_index", label: "Drop this index", template: "DROP INDEX {index};" },
];

/// One live view of a server, offered by name.
///
/// `name` is what the client sends back to `instance.inspect`; `label` is what
/// a person reads. `act` says whether a row can be ACTED on — terminating a
/// session is the one thing these views are for besides reading, and a view
/// that offers it on a catalogue that cannot be written to would be a button
/// that always fails.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Inspector {
    pub name: &'static str,
    pub label: &'static str,
    /// What acting on a row DOES, as a word a person reads: `terminate`, or
    /// empty where nothing can be done. The engine decides what that word
    /// means per driver; the client only shows it and asks.
    pub act: &'static str,
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
    /// Every value in this spec that must never appear in a message.
    ///
    /// `target()` keeps credentials OUT of the text the engine writes. This is
    /// the other half: taking them out of text the engine did not write. A
    /// driver's failure, an SSH library's failure and a database's own error
    /// are all assembled elsewhere and routinely quote back the value they
    /// choked on — that is how a passphrase typed into the key-path field
    /// reached an audit log in full, inside `cannot load ssh key '…'`.
    ///
    /// So: the path counts. Not because a path is secret, but because the
    /// engine cannot tell a path from a passphrase typed where a path goes, and
    /// the one time it is wrong is the one time it matters. A field that holds
    /// a credential when somebody makes a mistake is a credential field.
    ///
    /// Literal values only. A credential that arrives through `{{ env }}`,
    /// `{{ cmd }}` or the keyring is resolved deep inside a driver, and this
    /// never sees it — see the note on `Secret::resolve`.
    pub fn sensitive(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut add = |s: &str| {
            let s = s.trim();
            // Below four characters a replacement does more damage to the
            // sentence than the value did, and "ab" is not a credential.
            if s.len() >= 4 {
                out.push(s.to_string());
            }
        };
        match &self.auth {
            AuthSpec::Password { password, .. } => add(password.exposed()),
            AuthSpec::ClientCert { cert, key, key_password, .. } => {
                // Same rule: a certificate path is diagnostic, a value that is
                // not shaped like one is somebody's secret in the wrong box.
                for p in [cert, key] {
                    if !lode_proto::looks_like_path(p.original()) {
                        add(p.original());
                        add(p.as_str());
                    }
                }
                add(key_password.exposed());
            }
            AuthSpec::Provider { token, .. } => add(token.exposed()),
            AuthSpec::None | AuthSpec::Kerberos { .. } => {}
        }
        if let Some(t) = &self.tunnel {
            match &t.auth {
                TunnelAuth::Key { path, passphrase } => {
                    // A path that LOOKS like a path is shown. `no such file` is
                    // useless without the name of the file, and that name is the
                    // one genuinely helpful thing in the message.
                    //
                    // A value that is not shaped like a path is a passphrase
                    // somebody typed into the path box — the mistake that put a
                    // credential in an audit log — and stays hidden. Shape is
                    // what separates the two, and the shape rule is
                    // `lode_proto::looks_like_path`, the same one the connection
                    // window refuses a save with, so there is one answer to
                    // "is this a path" rather than one per end.
                    if !lode_proto::looks_like_path(path.original()) {
                        add(path.original());
                        add(path.as_str());
                    }
                    add(passphrase.exposed());
                }
                TunnelAuth::Password { password } => add(password.exposed()),
                TunnelAuth::Agent => {}
            }
        }
        out
    }

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

    /// A required param that names a FILE, expanded.
    ///
    /// The params map is free-form text, so it cannot carry `FsPath` the way a
    /// typed field does. This is the single place a file parameter is read, so
    /// a driver cannot take one without the expansion — `sqlite` and `duckdb`
    /// each remembered to call `expand_tilde` and the next one would have had
    /// to remember too.
    pub fn param_path(&self, key: &str) -> anyhow::Result<FsPath> {
        Ok(FsPath::new(self.param(key)?))
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
        cert: FsPath,
        key: FsPath,
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
    pub ca: Option<FsPath>,
    #[serde(default)]
    pub client_cert: Option<FsPath>,
    #[serde(default)]
    pub client_key: Option<FsPath>,
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
        path: FsPath,
        #[serde(default)]
        passphrase: Secret,
    },
    Password {
        #[serde(default)]
        password: Secret,
    },
}

// ── result model (uniform across every driver) ───────────────────────────────

/// What a column's values ARE, as opposed to what its driver calls them.
///
/// Eighteen drivers have eighteen vocabularies for the same eight ideas:
/// `int8`, `BIGINT`, `Int64` and `NUMBER(38,0)` are one kind, and a client that
/// wants to know whether it can put a column on a time axis should not have to
/// learn all four. So the engine says which of the eight it is.
///
/// Derived from the TYPE NAME rather than filled in by each driver, and in one
/// table rather than eighteen: the names are the drivers' own words, one table
/// can be read and tested at once, and a driver that declares nothing — SQLite
/// often, Mongo always — gets `unknown` instead of a guess. A client that
/// wants more than this still has the values.
pub fn kind_of(type_name: &str) -> &'static str {
    let t = type_name.trim().to_ascii_lowercase();
    if t.is_empty() {
        return "unknown";
    }
    // The part before a size or a precision: `varchar(255)`, `numeric(38,0)`,
    // `Nullable(Int64)` — the inside of a ClickHouse wrapper is the type, so
    // the brackets are peeled rather than cut.
    let bare = t
        .trim_start_matches("nullable(")
        .trim_start_matches("lowcardinality(")
        .trim_end_matches(')');
    let head = bare.split(['(', '[', '<', ' ']).next().unwrap_or(bare);
    // ORDER MATTERS: `timestamp` must be met before `time`, and `datetime64`
    // before `date`, or a timestamp column would be filed as a date.
    const TIMESTAMP: &[&str] = &[
        "timestamp", "timestamptz", "datetime", "datetime2", "datetime64", "smalldatetime",
        "date", "time", "timetz", "datetimeoffset", "interval", "year",
    ];
    const INTEGER: &[&str] = &[
        "int", "int2", "int4", "int8", "int16", "int32", "int64", "int128", "int256", "integer",
        "smallint", "bigint", "tinyint", "mediumint", "serial", "serial2", "serial4", "serial8",
        "smallserial", "bigserial", "uint8", "uint16", "uint32", "uint64", "uint128", "uint256",
        "long", "counter", "number", "hugeint", "ubigint", "usmallint", "utinyint", "uinteger",
    ];
    const FLOAT: &[&str] = &[
        "float", "float4", "float8", "float32", "float64", "double", "real", "numeric",
        "decimal", "money", "smallmoney", "dec", "fixed",
    ];
    const BOOL: &[&str] = &["bool", "boolean", "bit"];
    const JSON: &[&str] = &["json", "jsonb", "document", "object", "map", "variant"];
    const BINARY: &[&str] =
        &["bytea", "blob", "binary", "varbinary", "bytes", "image", "fixedstring", "raw"];
    const TEXT: &[&str] = &[
        "text", "varchar", "char", "nchar", "nvarchar", "ntext", "character", "string", "uuid",
        "name", "citext", "clob", "enum", "set", "inet", "cidr", "macaddr", "xml", "ascii",
        "varchar2", "nvarchar2", "lowcardinality",
    ];
    for (names, kind) in [
        (TIMESTAMP, "timestamp"),
        (INTEGER, "integer"),
        (FLOAT, "float"),
        (BOOL, "bool"),
        (JSON, "json"),
        (BINARY, "binary"),
        (TEXT, "text"),
    ] {
        if names.contains(&head) {
            return kind;
        }
    }
    // A name nothing matched: `unknown`, not `text`. Calling an unrecognised
    // type text is how a client ends up charting a geometry column.
    "unknown"
}

/// One result column.
#[derive(Debug, Clone, Deserialize)]
pub struct Column {
    pub name: String,
    /// What the engine calls this column's type, or empty when it does not say.
    ///
    /// Optional on the way IN — which only an external driver uses — because a
    /// driver that does not know a column's type should say nothing rather than
    /// invent a word. The grid already treats an unknown type as unknown:
    /// SQLite usually declares nothing either, and what can be charted is
    /// decided from the VALUES rather than from this.
    #[serde(rename = "type", default)]
    pub type_name: String,
}

/// Written by hand, so `kind` goes out beside the type without every driver
/// having to fill it in.
///
/// Eighteen drivers build this struct, several of them in two places. Adding a
/// field would have meant touching all of them and giving each one a chance to
/// disagree about what `int8` is; deriving it here means one answer.
impl Serialize for Column {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut o = s.serialize_struct("Column", 3)?;
        o.serialize_field("name", &self.name)?;
        o.serialize_field("type", &self.type_name)?;
        o.serialize_field("kind", kind_of(&self.type_name))?;
        o.end()
    }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    /// Part of the object's identifying key (a PK column; mongo's `_id`; a CQL partition/clustering key).
    pub primary: bool,
    /// What this column POINTS AT, when it is a foreign key.
    ///
    /// The single most useful thing a client can say about a column and the
    /// one thing it cannot work out for itself: `customer_id` looks like a
    /// reference and `parent` does not, and both guesses are wrong about half
    /// the schemas in the world. Only the server knows, and it is asked here
    /// rather than guessed at from the name.
    ///
    /// `None` where there is no reference, and also where the driver has no
    /// way to ask — a key/value store has no constraints, and saying nothing
    /// is the honest answer rather than saying no.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<Ref>,
}

/// Where a foreign key points.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ref {
    /// The object referred to.
    pub table: String,
    /// Its schema, where the engine has them and the reference crosses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// The column referred to. A composite key points at several, and this
    /// carries ONE: the client's use for it is "open that row", and a
    /// composite reference is drawn as the several rows it is.
    pub column: String,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

/// A reference to a browsable object (for `schema.columns` / `schema.indexes` / `schema.ddl`).
#[derive(Debug, Clone, Deserialize, Serialize)]
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

/// A path out of a spec, expanded ONCE, where it is read.
///
/// Two readers forgot: `net.rs` handed the SSH key's path straight to
/// `load_secret_key`, and `tls.rs` read the certificate and the key with
/// `fs::read`, while `server.rs` expanded in four separate places. So
/// `~/.ssh/neterra` — a file that exists — came back as "no such file", which
/// is a lie about somebody's disk and sends them looking in the wrong place.
///
/// Expanding at USE means every new reader has to remember. Expanding at
/// DESERIALIZE means none of them can forget, because by the time a reader sees
/// the value there is no `~` left in it. That is the whole reason this is a type
/// and not a function.
///
/// The original is kept and is what goes back out: a connection saved with
/// `~/.ssh/id_ed25519` should still say that when it is read on another machine
/// or by another person, rather than being quietly rewritten to one home
/// directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsPath {
    /// What was typed, and what is written back out.
    original: String,
    /// What the filesystem is actually asked for.
    expanded: String,
}

impl FsPath {
    pub fn new(original: impl Into<String>) -> FsPath {
        let original = original.into();
        let expanded = expand_tilde(&original);
        FsPath { original, expanded }
    }

    /// The path to open. Deliberately the only cheap way to get a `&str` out.
    pub fn as_str(&self) -> &str {
        &self.expanded
    }

    /// What somebody typed — for a message, and for deciding what may be shown.
    pub fn original(&self) -> &str {
        &self.original
    }

    pub fn is_empty(&self) -> bool {
        self.original.trim().is_empty()
    }
}

impl std::ops::Deref for FsPath {
    type Target = str;
    fn deref(&self) -> &str {
        &self.expanded
    }
}

impl std::fmt::Display for FsPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.expanded)
    }
}

impl AsRef<std::path::Path> for FsPath {
    fn as_ref(&self) -> &std::path::Path {
        std::path::Path::new(&self.expanded)
    }
}

impl<'de> Deserialize<'de> for FsPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<FsPath, D::Error> {
        Ok(FsPath::new(String::deserialize(d)?))
    }
}

impl Serialize for FsPath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.original)
    }
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

/// The inverse of the serializer above, for values coming IN — from an external
/// driver, which is the only thing that sends them.
///
/// Symmetric on purpose: the tagged forms this engine emits (`__oid`, `__ext`,
/// `__int`, `__bytes`) are read back as the types they stand for, so a driver
/// that echoes a row it was given returns the same row and not a row of
/// strings. Everything else maps the plain JSON way, which is what a driver
/// writing rows for the first time will send without reading any of this.
impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
        let j = serde_json::Value::deserialize(d)?;
        Ok(Value::from_json(j))
    }
}

impl Value {
    /// Plain JSON, and the engine's own tags, read as the types they mean.
    pub fn from_json(j: serde_json::Value) -> Value {
        match j {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else {
                    Value::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(t) => Value::Text(t),
            serde_json::Value::Object(ref m) => {
                if let Some(h) = m.get("__oid").and_then(|x| x.as_str()) {
                    return Value::Oid(h.to_string());
                }
                if let Some(d) = m.get("__int").and_then(|x| x.as_str()) {
                    // A big integer came across as digits so it would not go
                    // through a double. Putting it back through one here would
                    // undo the whole point of the tag.
                    if let Ok(i) = d.parse::<i64>() {
                        return Value::Int(i);
                    }
                    return Value::Text(d.to_string());
                }
                if let Some(b) = m.get("__bytes").and_then(|x| x.as_str()) {
                    let len = m.get("len").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                    return Value::Bytes { b64: b.to_string(), len };
                }
                if let (Some(k), Some(t)) = (
                    m.get("__ext").and_then(|x| x.as_str()),
                    m.get("v").and_then(|x| x.as_str()),
                ) {
                    return Value::Ext { text: t.to_string(), key: k.to_string() };
                }
                Value::Json(j)
            }
            other => Value::Json(other),
        }
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

    /// Eighteen vocabularies, eight ideas.
    ///
    /// The point of `kind_of` is that a client asking "can this go on a time
    /// axis" does not have to know that Postgres says `timestamptz`, MySQL
    /// `datetime`, ClickHouse `DateTime64(3)` and SQL Server
    /// `datetimeoffset`. Each line here is one of those words from a real
    /// driver.
    #[test]
    fn one_kind_for_every_name_the_drivers_use() {
        use super::kind_of;
        for t in [
            "timestamp",
            "timestamptz",
            "timestamp with time zone",
            "DateTime64(3)",
            "datetimeoffset",
            "DATE",
            "time",
            "interval",
        ] {
            assert_eq!(kind_of(t), "timestamp", "{t}");
        }
        for t in ["int4", "BIGINT", "Int64", "smallserial", "tinyint", "NUMBER", "UInt8"] {
            assert_eq!(kind_of(t), "integer", "{t}");
        }
        for t in ["float8", "double precision", "Float32", "numeric(38,2)", "money", "REAL"] {
            assert_eq!(kind_of(t), "float", "{t}");
        }
        for t in ["bool", "BOOLEAN", "bit"] {
            assert_eq!(kind_of(t), "bool", "{t}");
        }
        for t in ["json", "jsonb", "Map(String, UInt64)", "variant"] {
            assert_eq!(kind_of(t), "json", "{t}");
        }
        for t in ["bytea", "BLOB", "varbinary(max)", "FixedString(16)"] {
            assert_eq!(kind_of(t), "binary", "{t}");
        }
        for t in ["text", "varchar(255)", "uuid", "NVARCHAR(50)", "String", "citext"] {
            assert_eq!(kind_of(t), "text", "{t}");
        }
        // A WRAPPER is peeled rather than cut: the inside is the type.
        assert_eq!(kind_of("Nullable(Int64)"), "integer");
        assert_eq!(kind_of("LowCardinality(String)"), "text");
        // Nothing declared is UNKNOWN, and so is a name nothing matched:
        // calling an unrecognised type text is how a client charts a geometry
        // column.
        assert_eq!(kind_of(""), "unknown");
        assert_eq!(kind_of("   "), "unknown");
        assert_eq!(kind_of("geometry"), "unknown");
        assert_eq!(kind_of("tsvector"), "unknown");
        // ORDER: `timestamp` before `time`, `datetime64` before `date`.
        assert_eq!(kind_of("timestamp"), "timestamp");
        assert_eq!(kind_of("DateTime64"), "timestamp");
    }

    /// Every DDL template a driver declares can be filled in, and none of them
    /// is a statement with a hole left in it.
    ///
    /// The placeholders are the contract: a client substitutes `{qualified}`,
    /// `{table}`, `{column}`, `{type}`, `{to}`, `{index}` and `{columns}` and
    /// nothing else. A template with a `{something}` nobody fills in would
    /// reach the editor with braces in it, and a person would run it.
    #[test]
    fn a_ddl_template_has_no_placeholder_nobody_fills() {
        const KNOWN: &[&str] =
            &["{qualified}", "{table}", "{schema}", "{column}", "{type}", "{to}", "{index}", "{columns}"];
        let mut seen = 0;
        for m in crate::registry::Registry::new().metas() {
            let mut ops = std::collections::BTreeSet::new();
            for d in m.ddl_ops {
                assert!(!d.op.is_empty(), "{}: an op with no name", m.kind);
                assert!(!d.label.is_empty(), "{}: {} has no label", m.kind, d.op);
                assert!(ops.insert(d.op), "{}: {} twice", m.kind, d.op);
                // A statement, not a fragment: it ends in a semicolon, so
                // pasting two of them into an editor gives two statements.
                assert!(
                    d.template.trim_end().ends_with(';'),
                    "{}: {} does not end a statement: {:?}",
                    m.kind,
                    d.op,
                    d.template
                );
                let mut rest = d.template.to_string();
                for k in KNOWN {
                    rest = rest.replace(k, "");
                }
                assert!(
                    !rest.contains('{'),
                    "{}: {} has a placeholder nothing fills: {:?}",
                    m.kind,
                    d.op,
                    d.template
                );
                seen += 1;
            }
        }
        assert!(seen > 0, "no driver in this build declares a schema change");
    }

    /// A driver that OFFERS acting on a view says what acting is called.
    ///
    /// An empty `act` means "this view is read only", and that is a different
    /// thing from a view whose action is nameless: a button with no word on it
    /// is a button nobody presses twice. The rule is checked here because it
    /// is the one invariant a client relies on — it draws the word.
    #[test]
    fn a_live_view_names_its_action_or_offers_none() {
        let mut seen = 0;
        for m in crate::registry::Registry::new().metas() {
            let mut names = std::collections::BTreeSet::new();
            for i in m.inspectors {
                assert!(!i.name.is_empty(), "{}: a view with no name", m.kind);
                assert!(!i.label.is_empty(), "{}: {} has no label", m.kind, i.name);
                assert!(
                    i.name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                    "{}: {:?} is not a stable name",
                    m.kind,
                    i.name
                );
                // One name per driver: two views answering to one name is one
                // view, and which one is whichever the search found first.
                assert!(names.insert(i.name), "{}: {} twice", m.kind, i.name);
                seen += 1;
            }
        }
        // The drivers that have views are built in this configuration; if none
        // were, this test would pass by saying nothing.
        assert!(seen > 0, "no driver in this build declares a live view");
    }

    /// The kind goes out WITH the column, without eighteen drivers filling it.
    #[test]
    fn a_column_says_its_kind_on_the_wire() {
        let c = super::Column { name: "ts".into(), type_name: "timestamptz".into() };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["name"], serde_json::json!("ts"));
        assert_eq!(v["type"], serde_json::json!("timestamptz"));
        assert_eq!(v["kind"], serde_json::json!("timestamp"));
        // And a driver that declares nothing says so rather than guessing.
        let bare = super::Column { name: "x".into(), type_name: String::new() };
        assert_eq!(serde_json::to_value(&bare).unwrap()["kind"], serde_json::json!("unknown"));
    }

    /// A field is drawn from what the driver DECLARES, not from its key.
    ///
    /// The client offers the names out of `~/.aws/config` for any parameter
    /// typed `aws_profile`. If it matched the key instead, the next driver to
    /// want one would have to be named in the client — and the client would be
    /// guessing at a meaning rather than reading one.
    #[test]
    fn a_parameter_that_wants_an_aws_profile_says_so_in_its_type() {
        let reg = crate::registry::Registry::new();
        let mut declared = 0usize;
        for m in reg.metas() {
            for p in m.params {
                if p.key == "aws_profile" {
                    declared += 1;
                    assert!(
                        matches!(p.kind, ParamType::AwsProfile),
                        "{}: the {} parameter is typed {:?}, so no client can know what it is",
                        m.kind,
                        p.key,
                        p.kind
                    );
                }
                // And nothing else claims the type by accident.
                if matches!(p.kind, ParamType::AwsProfile) {
                    assert_eq!(
                        p.key, "aws_profile",
                        "{}: {} is typed as an AWS profile",
                        m.kind, p.key
                    );
                }
            }
        }
        assert!(declared >= 3, "expected s3, dynamodb and cloudwatch; found {declared}");

        // It must survive the wire as a word a client can match on, since that
        // is the whole mechanism.
        let json = serde_json::to_value(ParamSpec {
            key: "aws_profile",
            label: "…or an AWS profile",
            kind: ParamType::AwsProfile,
            required: false,
            secret: false,
            default: None,
        })
        .expect("serializes");
        assert_eq!(json["type"], "aws_profile");
    }

    /// A `~` reaches the filesystem expanded, from EVERY field that holds a
    /// path.
    ///
    /// Two readers forgot independently — `net.rs` handed the SSH key's path
    /// raw to `load_secret_key`, `tls.rs` read the certificate and key with
    /// `fs::read` — while `server.rs` expanded in four other places. A real file
    /// came back as "no such file", which is a lie about somebody's disk.
    ///
    /// The fields are typed, so this asserts the TYPE does it. One test covers
    /// every reader present and every reader to come, which is the point of
    /// having moved it to deserialization.
    #[test]
    fn a_tilde_is_expanded_in_every_path_a_spec_carries() {
        let home = std::env::var("HOME").expect("a home directory");
        let spec: ConnSpec = serde_json::from_value(serde_json::json!({
            "driver": "mongodb",
            "params": { "host": "db.example", "database": "app" },
            "auth": {
                "kind": "client_cert",
                "cert": "~/.certs/client.pem",
                "key": "~/.certs/client.key",
                "user": "u"
            },
            "tls": {
                "mode": "verify_full",
                "ca": "~/.certs/ca.pem",
                "client_cert": "~/.certs/client.pem",
                "client_key": "~/.certs/client.key"
            },
            "tunnel": {
                "host": "bastion.example",
                "user": "me",
                "auth": { "kind": "key", "path": "~/.ssh/neterra", "passphrase": "" }
            }
        }))
        .expect("the spec parses");

        // The tunnel key — the one that was actually reported.
        let tunnel = spec.tunnel.as_ref().expect("a tunnel");
        match &tunnel.auth {
            TunnelAuth::Key { path, .. } => {
                assert_eq!(path.as_str(), format!("{home}/.ssh/neterra"));
                // And what goes back out is still what was typed, so a saved
                // connection does not get rewritten to one person's home.
                assert_eq!(path.original(), "~/.ssh/neterra");
            }
            other => panic!("expected a key tunnel, got {other:?}"),
        }

        // TLS: the CA bundle and both client files.
        assert_eq!(
            spec.tls.ca.as_ref().map(|p| p.as_str().to_string()),
            Some(format!("{home}/.certs/ca.pem"))
        );
        assert_eq!(
            spec.tls.client_cert.as_ref().map(|p| p.as_str().to_string()),
            Some(format!("{home}/.certs/client.pem"))
        );
        assert_eq!(
            spec.tls.client_key.as_ref().map(|p| p.as_str().to_string()),
            Some(format!("{home}/.certs/client.key"))
        );

        // Client-certificate auth.
        match &spec.auth {
            AuthSpec::ClientCert { cert, key, .. } => {
                assert_eq!(cert.as_str(), format!("{home}/.certs/client.pem"));
                assert_eq!(key.as_str(), format!("{home}/.certs/client.key"));
            }
            other => panic!("expected client_cert auth, got {other:?}"),
        }

        // And a driver's own file parameter, which is free-form text and so
        // goes through the one accessor instead of a type.
        let file_spec: ConnSpec = serde_json::from_value(serde_json::json!({
            "driver": "sqlite",
            "params": { "file": "~/data/app.db" }
        }))
        .expect("parses");
        assert_eq!(
            file_spec.param_path("file").expect("a file").as_str(),
            format!("{home}/data/app.db")
        );
    }

    /// A path is SHOWN; a secret typed into a path field is not.
    ///
    /// Hiding the path made the one useful part of `no such file` unreadable —
    /// somebody could not see which path was tried, which is the only thing that
    /// message is for. Hiding it only when it is not shaped like a path keeps
    /// the ordinary case diagnosable and the mistaken one covered.
    #[test]
    fn a_real_path_stays_readable_and_a_misplaced_secret_does_not() {
        let with_path: ConnSpec = serde_json::from_value(serde_json::json!({
            "driver": "mongodb",
            "params": {},
            "tunnel": {
                "host": "b.example", "user": "me",
                "auth": { "kind": "key", "path": "~/.ssh/neterra", "passphrase": "" }
            }
        }))
        .expect("parses");
        let guard = with_path.sensitive();
        let said = format!(
            "cannot load ssh key '{}': No such file or directory",
            std::env::var("HOME").unwrap_or_default() + "/.ssh/neterra"
        );
        let out = crate::audit::scrub(&said, &guard);
        assert!(out.contains(".ssh/neterra"), "the path must stay readable: {out}");

        // The same field holding what is plainly not a path.
        let phrase = "correct-horse!battery#staple$42";
        let with_secret: ConnSpec = serde_json::from_value(serde_json::json!({
            "driver": "mongodb",
            "params": {},
            "tunnel": {
                "host": "b.example", "user": "me",
                "auth": { "kind": "key", "path": phrase, "passphrase": "" }
            }
        }))
        .expect("parses");
        let guard2 = with_secret.sensitive();
        let said2 = format!("cannot load ssh key '{phrase}': No such file or directory");
        let out2 = crate::audit::scrub(&said2, &guard2);
        assert!(!out2.contains(phrase), "a misplaced secret must stay hidden: {out2}");
    }

    /// The reported leak, as a test.
    ///
    /// Somebody typed a passphrase into the key-PATH field. The SSH layer could
    /// not open a file by that name and said so, quoting it, and the sentence
    /// went into the audit log in full. The assertion is not that the message
    /// is pretty — it is that the value is in NO field of the record.
    #[test]
    fn a_value_from_a_secret_field_reaches_no_field_of_the_record() {
        let passphrase = "correct-horse!battery#staple$42";
        let spec: ConnSpec = serde_json::from_value(serde_json::json!({
            "driver": "mongodb",
            "name": "neterra-mongo",
            "params": { "host": "db.example", "database": "app" },
            "auth": { "kind": "password", "user": "u", "password": "hunter2-and-then-some" },
            "tunnel": {
                "host": "bastion.example",
                "user": "me",
                "auth": { "kind": "key", "path": passphrase, "passphrase": "" }
            }
        }))
        .expect("the spec parses");

        let guard = spec.sensitive();
        assert!(guard.iter().any(|v| v == passphrase), "the path is a guarded value");

        // The message an SSH library actually produces, with the value in it.
        let said = format!("cannot load ssh key '{passphrase}': No such file or directory");
        let mut r = crate::audit::Record::new("conn.connect", "failed");
        r.conn = spec.name.clone();
        r.driver = Some(spec.driver.clone());
        // `target()` needs a driver's metadata; the registry has it, and the
        // question here is about the message rather than about the driver.
        let reg = crate::registry::Registry::new();
        let meta = reg
            .metas()
            .into_iter()
            .find(|m| m.kind == "mongodb")
            .expect("mongodb is registered");
        r.statement = spec.target(meta);
        r.guarding(guard.clone());
        r.because(said);

        for (field, text) in [
            ("reason", r.reason.clone().unwrap_or_default()),
            ("statement", r.statement.clone()),
            ("conn", r.conn.clone().unwrap_or_default()),
            ("driver", r.driver.clone().unwrap_or_default()),
            ("outcome", r.outcome.clone()),
        ] {
            assert!(!text.contains(passphrase), "{field} still holds the value: {text}");
            assert!(
                !text.contains("hunter2-and-then-some"),
                "{field} still holds the password: {text}"
            );
        }
        // And it still says something: a record scrubbed to nothing is no
        // better than no record.
        assert!(r.reason.as_deref().unwrap_or("").contains("cannot load ssh key"));
    }
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
