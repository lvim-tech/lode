# lode

**lode** is a database engine for editors and desktop clients: one process that
owns the drivers, the connections, the running statements and the saved state,
and answers newline-delimited JSON-RPC on its stdin/stdout.

It is not a library and not a server. A client spawns `loded` as a child
process and talks to it over the pipe. That shape is the point:

- **Crash isolation.** Database drivers carry C dependencies. A driver panic or
  a C-level abort takes down `loded`, not the editor or the GUI — the client
  notices the pipe close and respawns.
- **One engine, many clients.** The same binary backs the
  [`lvim-db`](https://github.com/lvim-tech/lvim-db) Neovim plugin and the lode
  desktop client in this repository. Connections, saved queries and history live
  in the engine, so both see the same state.
- **Secrets never reach the client.** A connection stores credentials as
  *templates* (`{{ env "PGPASSWORD" }}`, `{{ vault "prod/db" }}`). The engine
  resolves them at connect time — from the environment, from a file, or from the
  [`lvim-keyring`](https://github.com/lvim-tech/lvim-keyring) agent — and a
  resolved value is never written to a response, a log line or the store.
- **No blocking.** A tokio multi-thread runtime drives every driver; each
  request is its own task; a long query never blocks the handshake, another
  connection, or its own cancel.

## Build

Needs a Rust toolchain (`cargo`). Nothing else: TLS is rustls, the SSH tunnel is
russh, SQLite and DuckDB are bundled — no OpenSSL, no system client libraries.

```sh
scripts/build                       # release build → build/loded
scripts/build --features oracle     # plus an opt-in driver
```

`scripts/build` installs the binary at `build/loded`, which is where clients
probe for it. A plain `cargo build --release` leaves it at
`target/release/loded`; clients probe that too.

## Drivers

Fourteen kinds in the default build. Each is a cargo feature, so a build can
carry only the set it needs (`cargo build --release --no-default-features
--features sqlite,postgres`).

| kind | engine | notes |
|---|---|---|
| `postgres` | PostgreSQL | `tokio-postgres`, rustls |
| `cockroachdb` | CockroachDB | postgres wire, own defaults |
| `mariadb` | MariaDB | `mysql_async`, rustls |
| `mysql` | MySQL | same impl, distinct kind |
| `sqlserver` | Microsoft SQL Server | TDS via `tiberius` |
| `sqlite` | SQLite | bundled, file-backed |
| `duckdb` | DuckDB | bundled, file-backed |
| `mongodb` | MongoDB | official driver |
| `redis` | Redis | key/value, no schema tree |
| `clickhouse` | ClickHouse | HTTP `JSONCompact` |
| `snowflake` | Snowflake | HTTP SQL REST API, key-pair JWT |
| `cassandra` | Cassandra | CQL via `scylla` |
| `scylla` | ScyllaDB | same impl, distinct kind |
| `firebird` | Firebird | pure-Rust wire protocol |

Opt-in features that need a system library at runtime stay out of the default
set: `firebird-native` (libfbclient).

Every TCP driver can be fronted by an **SSH tunnel** (`tunnel` feature) with no
per-driver code — the tunnel forwards the socket through an SSH `direct-tcpip`
channel, so it works for drivers that have no TLS of their own.

## The protocol

One JSON object per line, on `loded`'s stdin and stdout. A client sends
**requests**; the engine sends one **response** per request (correlated by `id`)
plus unsolicited **notifications** (no `id`).

```
request        {"id":N,"method":"…","params":{…}}
response ok    {"id":N,"ok":true,"result":{…}}
response err   {"id":N,"ok":false,"error":"…"}
notification   {"method":"…","params":{…}}
```

`proto` grows **additively**: a client accepts any engine reporting
`proto >= its minimum`, so a newer engine keeps working with an older client.
The current protocol is **13**. What each version added is listed in
`src/rpc.rs` beside the constant.

### Handshake

| method | result |
|---|---|
| `rpc.hello` | `{ engine, version, proto, drivers }` |

`drivers` is the list of `DriverMeta` — kind, display name, default port, the
typed parameter fields, the accepted auth methods, and a capability set
(`sql`, `schemas`, `cancel`, `tls`, `tunnel`, `multi_db`, `kv`, `indexes`,
`ddl`). **A client builds its connection form from this**, so adding a database
type never edits any client code.

### Connections

| method | params | result |
|---|---|---|
| `conn.connect` | a `ConnSpec` | `{ conn_id, encrypted, tunneled }` |
| `conn.test` | `{ stage, spec }` | `{ ok, ms, detail }` |
| `conn.disconnect` | `{ conn_id }` | `{}` |
| `conn.databases` | `{ conn_id }` | `{ databases }` |
| `conn.switch_database` | `{ conn_id, database }` | `{}` |

A `ConnSpec` is `{ driver, params, auth, tls, tunnel }`. `params` is a string
map keyed by the driver's own `ParamSpec` keys; `auth` is one of `none`,
`password`, `client_cert`, `provider`, `kerberos`; secret-bearing fields hold
templates, never values.

`conn.test` dry-runs ONE layer so a failure is attributed to the layer that
caused it: `endpoint` (stat the file, or reach the server), `tunnel` (stand the
SSH forward up alone), `tls` and `auth` (a real connect, closed again).

On a TUNNELLED spec the endpoint stage asks the SSH server to open a
`direct-tcpip` channel to the endpoint and reports what it said. It deliberately
does not dial the local forwarded port: that listener belongs to this process, so
connecting to it proves only that the SSH session came up — which is how a
MongoDB connection aimed at a port with nothing on it once reported `reachable
through the SSH tunnel` in 251 ms.

An **unencrypted** link is never silent: `conn.connect` emits a `daemon.log`
warning and returns `encrypted: false`.

### Schema

| method | params | result |
|---|---|---|
| `schema.structure` | `{ conn_id }` | `{ nodes }` — the object tree |
| `schema.columns` | `{ conn_id, object }` | `{ columns }` |
| `schema.indexes` | `{ conn_id, object }` | `{ indexes }` |
| `schema.ddl` | `{ conn_id, object }` | `{ ddl }` or `null` |

`indexes` and `ddl` are offered only where the driver's `caps` claim them — a
key/value store has neither, and several SQL engines expose indexes but no
server-side `CREATE` statement.

### Queries

| method | params | result |
|---|---|---|
| `query.execute` | `{ conn_id, statement }` | `{ call_id }` **immediately** |
| `query.page` | `{ call_id, from, n }` | `{ ready, columns, rows, from, has_more, total, affected }` |
| `query.cancel` | `{ call_id }` | `{}` |
| `query.release` | `{ call_id }` | `{}` |
| `query.cell` | `{ call_id, row, col }` | `{ value }` — one full cell |

`query.execute` answers with its `call_id` **before** the statement starts, so
the response provably precedes the `query.state` notification for that call.
The client then waits for the state and pages the result.

Rows are buffered on the engine as they are pulled, so paging **backwards** is
free and a cell's full value is always retrievable even when the grid truncated
it. `query.release` drops a finished call's buffer; a client that keeps the last
N calls alive gets an instant re-open of their results.

### Saved state (`proto >= 2`)

The engine owns the saved connections, the saved queries and the history, in its
own SQLite database — so two clients share them.

| method | params | result |
|---|---|---|
| `store.status` | — | `{ available, path, error }` |
| `store.connections.list` | — | `{ connections }` |
| `store.connections.get` | `{ name }` | `{ connection }` or `null` |
| `store.connections.save` | `{ name, driver, spec }` | `{ saved }` |
| `store.connections.remove` | `{ name }` | `{ removed }` |
| `store.queries.list` | `{ conn }` | `{ queries }` |
| `store.queries.get` | `{ conn, name }` | `{ query }` or `null` |
| `store.queries.save` | `{ conn, name, sql }` | `{ saved }` |
| `store.queries.remove` | `{ conn, name }` | `{ removed }` |
| `store.history.record` | `{ conn, driver, statement, state, ms, rows }` | `{ recorded }` |
| `store.history.list` | `{ limit }` | `{ history }` |
| `store.charts.list` | `{ conn }` | `{ charts }` |
| `store.charts.save` | `{ conn, name, sql, measure }` | `{ saved }` |
| `store.charts.remove` | `{ conn, name }` | `{ removed }` |
| `store.audit.list` | `{ limit, conn? }` | `{ audit, mode }` |
| `store.import` | `{ path }` | `{ connections, queries, history }` |

A saved connection keeps only the **template**: `spec` is the
`{ params, auth, tls, tunnel }` JSON with secrets still as templates. Nothing
resolved is ever stored.

`store.import` reads an older client's own SQLite file and copies what is
missing — keyed by `name` for connections, `(conn, name)` for queries, and
`(ts, statement)` for history — so running it twice imports nothing the second
time. The source file is only read.

The database lives at `$LODE_DATA_DIR/lode.db`, else
`$XDG_DATA_HOME/lode/lode.db`, else `~/.local/share/lode/lode.db`.

### The cloud drivers, and what is NOT verified about them

`redshift`, `libsql`, `s3`, `dynamodb`, `cloudwatch` and `influxdb` complete the
driver list. **None of their network halves has been run against a real server**,
because there is neither here. Rather than claim otherwise, each one splits in
two and only the untested half is untested:

| driver | verified here | NOT verified |
|---|---|---|
| `redshift` | that the alias registers and carries its own metadata | the wire, which is the postgres driver's |
| `libsql` | **the whole local half** — a libSQL file is a SQLite file, opened by the sqlite driver; and the Turso request/answer shapes | the HTTP round trip to Turso |
| `s3` | the command language, the request each command becomes, the AWS/R2/MinIO endpoint rules, the XML parsed into rows, the errors | the HTTP round trip |
| `dynamodb` | the request document, which call it becomes, every attribute type, documents with differing attributes, the errors | the HTTP round trip |
| `cloudwatch` | the command language, the time arithmetic in UTC, the request bodies, the answers | the HTTP round trip |
| `influxdb` | which version a connection is, both request shapes, both answer formats, and the danger rules for InfluxQL *and* Flux | the HTTP round trip |

The AWS **signing** is verified, and is the reason it is written out here rather
than taken from a crate: AWS publishes a canonical derivation, so the signing key
is checked against an answer somebody else computed. A signer tested against
itself proves only that it is consistent, which a wrong signer also is.

The four whose network half is untested **refuse to connect**, and say so in
those words. Offering a connection that has never been made would be exactly the
green that means nothing. `libsql` does not refuse for a local file, because that
half really is tested.

Two bugs the tests found before any server could have:

* Every Flux script begins `from(`, and the default SQL rule treats an
  unrecognised leading verb as a write — so a read-only connection would have
  refused every Flux **read**. Flux is now judged by its own rule, where `to()`
  is the only thing that writes.
* InfluxDB v2's annotated CSV has one fewer `#datatype` entry than a data line,
  because every line starts with InfluxDB's own empty column. Typing each raw
  field by its index was off by one the whole way across: `table` read as a date,
  `_time` as a number.

### External drivers (`proto >= 13`)

A program that speaks a small protocol on its stdio **is** a driver. It appears
in `rpc.hello`, builds its own connection form from its own metadata, its objects
appear in the tree, and both clients use it without knowing it is not built in —
the same principle the clients already follow (nothing knows a driver by name),
one level further out.

Register one as a `driver` profile whose spec is a command line:

```json
{"kind":"driver","name":"toy","spec":{"command":"python3 /path/to/toy.py"}}
```

It is asked to describe itself once at startup, with a short ceiling. A program
that cannot is **reported and skipped** — `hello.external` carries what became of
each one, so a driver that is installed and missing from the list always has a
message attached.

The protocol is newline JSON, one request at a time, on the child's stdio:

| method | params | result |
|---|---|---|
| `meta` | — | the driver's metadata |
| `connect` | `{ spec }` | `{ conn }` — any handle it likes |
| `structure` | `{ conn }` | `{ nodes }` |
| `columns` / `indexes` | `{ conn, object }` | `{ columns }` / `{ indexes }` |
| `execute` | `{ conn, statement }` | `{ columns, rows, affected? }` |
| `databases` / `switch_database` | `{ conn, … }` | optional |
| `close` | `{ conn }` | — |

Whole results, not streams: paging over a child's stdio with backpressure is a
lot of machinery for the long tail, and the engine already buffers pages for
everybody. A driver that returns a million rows returns a million rows — a thing
to know rather than to pretend about.

#### What it is not believed about

It is believed about **shape** — what its fields are called, what its statements
look like. Shape is harmless to be wrong about; a bad form is a bad form.

It is **not** believed about safety:

* `classify` and `writes` remain the engine's. A child cannot make a `DROP` safe,
  and a child answering "nothing writes" cannot turn a read-only connection into
  an open one.
* It cannot claim `caps.edit`. That is a promise that every value is bound as a
  parameter and that a write which hit more than one row can be undone — the
  engine cannot check it, so it will not repeat it. Nor `cancel` or `metrics`,
  which this transport does not have.
* It cannot take a built-in driver's name. One calling itself `postgres` would
  otherwise capture every saved postgres connection the moment it was registered.
* It is **not handed the credentials**. What crosses is the form's `params`, built
  explicitly rather than by serializing `ConnSpec`, so a field added there does
  not silently start being sent to every external driver.

A driver is a program somebody installed, and a program somebody installed is not
a reason to stop checking. The same shape as the MCP role: a floor, not a key.

### More than one language

The client's chrome — panels, buttons, headings — is translated. English and
Bulgarian today; `$LODE_LANG`, else `$LC_ALL` / `$LANG` read as a prefix, else
English, and a choice in Settings is remembered.

**The engine is not translated, and that is deliberate rather than unfinished.**
An engine message is a RECORD: "this connection is read only" goes into the
audit log, into a bug report, into a shell test, and into whatever somebody
pastes to somebody else. A log whose meaning depends on the locale of whoever
happened to run it cannot be read by the person who did not. Refusals, driver
errors and the audit log stay in English for the same reason SQL does: they are
the record, not the interface. The settings panel says so, so that a Bulgarian
window showing an English refusal reads as a decision rather than a gap.

A missing translation is caught by a test, not by a fallback:
`every_language_has_every_key` asserts each catalogue has exactly the keys
English has. A key added without a translation fails there rather than appearing
as a blank label three weeks later. Keys English does not have are failed too — a
translation for a key nothing uses is a line somebody maintains for ever.

### The MCP server (`proto >= 12`)

```
loded --mcp                  a reader: it cannot change anything
LODE_MCP_ROLE=writer loded --mcp
```

`loded --mcp` speaks the Model Context Protocol on stdio instead of lode's own.
It is the **same engine** underneath — same drivers, same store, same
`Driver::classify`, same connection policy, same audit log. There is no second,
softer path through any of it, because a second path is the one that gets used.

What differs only ever **restricts**, and there are three things:

1. **A role.** `reader` by default, and a reader refuses every write whatever the
   connection allows. The role is a floor, not a key; an unrecognised value in
   `$LODE_MCP_ROLE` is a reader, because a typo must not hand an agent a pen.
2. **`Policy::Open` is read as `Confirm`.** "Unrestricted" is a decision a person
   made about *themselves* — it means "stop asking me", not "let anything that
   can speak JSON-RPC drop a table". A permission granted to a person is not
   inherited by a machine acting in their name.
3. **The agent cannot confirm.** When `classify` names a statement it is NOT
   run: it is filed as an approval and the tool says so, in full, including why.

| method | params | result |
|---|---|---|
| `approvals.list` | `{ state?, limit? }` | `{ approvals }` |
| `approvals.decide` | `{ id, approve }` | `{ decided }` |

The queue is in the **store**, not in memory, because the two sides are
different processes: `loded --mcp` and whatever window the person has open. A
queue in memory would be one only the asker can see.

Asking twice files nothing new — an agent retrying in a loop is one question,
and one question is what a person has to answer. **A denial stands**: asking
again after a "no" does not file a fresh ask, or "no" would mean "not this
second" and the same question would come back for ever. An approval is **spent**
when it is used: they said yes to a DELETE, not to every DELETE from now on.

Every tool call is audited under `mcp.query`, refusals included. So is the
person's decision, under `approval` — "who let this through" is the question
asked straight after "what ran", and a log with the second and not the first
answers half of it.

The tools are `list_connections` (names and drivers only — never a host, never a
credential), `open`, `list_objects`, `describe`, `query` and `session`. Results
come back as tab-separated text with a row count: a model reads this, and JSON of
the same table is twice the tokens for the same information.

### Access providers (`proto >= 11`)

The AWS drivers all need the same thing, and nobody wants to paste an access key
into a connection form when the machine already has `~/.aws/config` with eight
profiles in it. The engine reads those files and offers what it finds.

| method | params | result |
|---|---|---|
| `access.list` | `{ config?, credentials? }` | `{ profiles, config, credentials }` |
| `access.test` | `{ profile, … }` | `{ ok, key_ends_with, has_session_token, region }` |
| `access.import` | `{ profile, as_name?, … }` | `{ saved, name, kind }` |

`$AWS_CONFIG_FILE` and `$AWS_SHARED_CREDENTIALS_FILE` are honoured, because the
AWS tools honour them and two answers to "where are the credentials" is one too
many. The reply says which files were read, so an empty list is a fact rather
than a mystery.

Each entry says what KIND it is and whether this engine can use it:

| kind | what it is | usable |
|---|---|---|
| `static` | keys in the file | yes |
| `process` | `credential_process` | yes |
| `inherited` | `source_profile` with no role | yes |
| `assume_role` | `role_arn` + `source_profile` | **no** |
| `sso` | `sso_session` / `sso_start_url` | **no** |

So a client can show the whole list and mark the ones that will not work, rather
than offering eight profiles of which three fail only when somebody picks them.

**The last two are refused by name, with what would have to happen instead** —
they need to talk to AWS and this engine does not. A silent "no credentials"
would read as a missing profile rather than an unsupported one. Nothing here is
verified against the real AWS: the parsing is verified against files, and the
network half does not exist rather than existing untested.

`access.test` answers with the access key id's **last four characters** and
nothing more. Enough to tell two profiles apart, not enough to be a credential —
the whole id is half of one, and a test button has no business handing it back
to be screenshotted.

An import stores the AWS profile's **NAME**, never its key. The credential stays
in the AWS files, where the AWS tools rotate it; copying it here would mean a key
that works today and a connection that fails next month for a reason nobody
connects to the rotation. Importing an unusable profile is refused on the way in.

The drivers that consume this arrive with the AWS drivers in stage 7; what is
here is the credential source and nothing claims otherwise.

### Hooks and scripts (`proto >= 10`)

A **hook** is the person's own code, run at one of four moments in a
connection's life. A **script** is a document in Bash, Python or Lua that they
run themselves. The same runner does both, because they are the same act seen
from two sides.

| method | params | result |
|---|---|---|
| `store.scripts.list` | `{ conn }` | `{ scripts }` — this connection's, and the global ones |
| `store.scripts.save` | `{ conn?, name, lang, body }` | `{ saved }` |
| `store.scripts.remove` | `{ conn?, name }` | `{ removed }` |
| `script.run` | `{ conn?, name }` or `{ body, lang }` | `{ run_id, name }` |
| `script.cancel` | `{ run_id }` | `{ cancelled }` |

Output streams as `script.output` `{ run_id, stream, text, hook? }` and the end
arrives as `script.state` `{ run_id, state, ms, code, error }`. The stream is
NAMED — stdout and stderr kept apart, because which one a message came from is
most of what a failing script is telling you.

`ConnSpec.hooks` is a list of `{ on, kind, source, lang?, timeout_ms? }`:

| `on` | when | a failure |
|---|---|---|
| `pre_connect` | before the dial | **stops the connection** |
| `post_connect` | once it is open | is reported, and does not undo it |
| `pre_disconnect` | while the link is still there | is reported |
| `post_disconnect` | after it is gone | is reported |

Only PreConnect blocks, and that is the only reason to have a hook before rather
than after. The others come *after* the thing they are named for, so there is
nothing left to stop — and refusing to disconnect because a hook failed would
leave a connection open that somebody asked to close.

`kind` is `command` (a command line, through a shell, so a pipe is a pipe),
`script` (the NAME of a saved script — by name, so a hook whose script was
deleted says which one rather than quietly doing nothing), or `inline` (the body,
with `lang`).

**The environment carries context and never a credential.** `LODE_EVENT`,
`LODE_CONN`, `LODE_DRIVER`, `LODE_HOST`, `LODE_PORT`, `LODE_DATABASE` — and
nothing from `auth`, `tls` or the tunnel, which are not read at all when it is
built rather than read and filtered. A child's environment is visible in `ps` and
in `/proc/<pid>/environ`, and to everything the child goes on to start. A hook
that needs a password fetches it from the keyring under its own name.

**Every hook and every script is in the audit log**, worked or not. A hook is
somebody's code running against their database on the engine's initiative, and
no honest reading of "what happened here" leaves that out.

A script's body goes in on **stdin**: not as an argument (length limits, and it
would show in `ps`) and not through a temporary file (a path somebody else can
read or replace between writing and running).

A run gets its **own process group**, so a timeout stops what the script started
and not merely the interpreter. Without that, `bash -` running `sleep 30` forks:
killing bash leaves `sleep` holding the output pipes, and a four-hundred-
millisecond timeout takes thirty seconds to report.

**Lua is not embedded**, and that is a limitation rather than a choice: an
in-process Lua needs `mlua`, which is not in this lock file and cannot be fetched
here, so `lua` runs as an interpreter like `python3` and `bash` do. An embedded
Lua could be given a sandbox and an API onto the open connection; an interpreter
can be given neither.

### Profiles and proxies (`proto >= 9`)

A **profile** is a reusable piece of network, by name. A bastion is one machine
and twenty connections go through it; written into twenty specs, rotating its
key means editing twenty of them and finding out which one was missed by its
failing.

| method | params | result |
|---|---|---|
| `store.profiles.list` | `{ kind }` | `{ profiles }` |
| `store.profiles.get` | `{ kind, name }` | `{ spec }` or `null` |
| `store.profiles.save` | `{ kind, name, spec }` | `{ saved }` |
| `store.profiles.remove` | `{ kind, name }` | `{ removed, was_used_by }` |

`kind` is `tunnel` or `proxy`. Profiles are NOT scoped to a connection — the
point is that several share one — and the two kinds are separate namespaces, so
a tunnel and a proxy may both be called `office`.

A spec is **parsed as the thing it claims to be** on the way in, so a malformed
tunnel is refused now rather than at three in the morning when a connection that
names it will not open. Credentials stay templates, exactly as in a connection.

A connection names one with `tunnel_profile` / `proxy_profile`. **A named
profile that is missing fails the connect.** It never falls back to a direct
link: a connection whose author asked for a bastion and silently got a plain
connection to the database is the one outcome that must not happen. Naming a
profile *and* spelling one out is refused rather than resolved by precedence —
precedence means one of the two silently losing, and a connection with two
tunnels is one whose author believed something untrue about it.

Removing a profile answers with `was_used_by`: it IS removed, because it is the
person's data, but a removal that takes several connections down without saying
so is a surprise rather than a choice.

#### Proxies

`proxy` is `{ kind, host, port, auth? }` with `kind` of `socks5` or
`http_connect`. It sits **under** everything: the database's dial and the SSH
server's dial both go through it, because a proxy that is sometimes bypassed is
not a rule. With both a proxy and a tunnel the order is
proxy → SSH server → direct-tcpip → database.

The host NAME goes over the wire, not an address. On the far side of a proxy the
client's DNS is usually the wrong DNS.

A proxy cannot be expressed as a `host:port` for a driver to dial, so an
untunnelled proxied connection gets a loopback forwarder — the same answer the
SSH tunnel already uses, and for the same reason: zero per-driver code. The first
dial happens before the port is handed over, so a listener that accepts and fails
on every connection is not what a driver is given.

`conn.test` gains a **`proxy`** stage. It asks the proxy for a name that cannot
exist (RFC 6761 `.invalid`), deliberately, because the question of the stage is
"does the proxy work": asking it for the database would make a refusal mean
either a broken proxy or a database that is down, which is exactly the
blame-sharing the staged tests exist to prevent. A proxy that answers *host
unreachable* has worked — it completed its handshake and gave a verdict.

### The audit log (`proto >= 7`)

`store.audit.list` returns, newest first, every operation that CHANGED a
database and every connection opened or closed — **including the attempts the
engine refused**. A log of what succeeded answers "what happened"; a log that
also holds what was stopped answers "what was tried", and the second question is
the one asked after something goes wrong.

The engine writes it, on the paths that do the work:

| `path` | written when |
|---|---|
| `execute` | a statement that `Driver::writes` calls a write — refused, applied, failed or cancelled |
| `row.write` | a single-row edit or delete |
| `conn.connect` | a connection opened, or an attempt that failed |
| `conn.disconnect` | a connection closed |

A read is never in it. Each row carries the connection's **policy as it was**,
whether the caller passed `confirm`, the outcome, the row count, the duration,
and the reason it was refused or failed.

There is deliberately **no method to delete from it**: a log a client can wipe
is a log of what that client chose to keep. Pruning it is the person's business,
with `sqlite3` against their own file.

**`fingerprint`** is a short stable name for the statement's SHAPE — literals
replaced, comments dropped, whitespace collapsed, identifiers kept. Two runs
that differ only in their values share one fingerprint, so "this same statement
ran four hundred times" is one group. It is not a seal: it is an identity for
grouping, and a hash written by the process that can also write the row would
look like integrity and be none.

**Credentials.** `statement` has the credential grammars redacted —
`PASSWORD '…'`, `IDENTIFIED BY '…'`, redis `AUTH` and `CONFIG SET requirepass`
all become `<redacted>`. Whatever the redactor removes is then removed from
every other field of the same row, which is what catches an engine that quotes
the failing statement back inside its error message (SQLite does, verbatim).
A connection's own secrets never reach the log at all: a `conn.connect` line is
assembled from the non-secret fields of the spec — driver, host, port, file,
TLS mode, tunnel hop — and the credential fields are never read.

A failure is the other half, and it is not ours to word. A driver, an SSH
library and a database each assemble their own error, and they quote back the
value they choked on. So a failed `conn.connect` or `conn.test` is scrubbed with
the spec's own credential values before it is written **or returned to the
client** — a clean log while the window shows the value in a toast is one place
fixed out of two. The SSH key **path** counts as a credential field here: not
because a path is secret, but because the engine cannot tell a path from a
passphrase typed where a path goes, and the one time it cannot tell is the one
time it matters.

A credential that arrives through `{{ env }}`, `{{ cmd }}` or the keyring is
resolved, used and dropped. While it is in scope — between the resolve and the
call that consumes it — a failure raised there is scrubbed with it. This covers
the SSH tunnel's password and key passphrase and the proxy's password.

> **Known limitation.** A resolved credential is NOT scrubbed from a message
> that arrives outside that scope. If a database echoes a password back in an
> error of its own, after the driver has the connection, that value can appear
> in the message and therefore in the log. Closing this would mean keeping every
> resolved secret in a process-wide register so any later message could be
> cleaned — which means plaintext passwords held for the lifetime of a daemon
> that runs for hours, ready-made for the next leak. The narrower leak is the
> better trade, so it is written down here rather than closed.

What this does **not** do is remove every value that happens to be sensitive. A
token written through `INSERT INTO t (tok) VALUES ('…')` is an ordinary value in
an ordinary place, and no pattern can tell it from a product name. So there is a
real switch rather than a promise:

| `LODE_AUDIT_STATEMENTS` | `statement` holds |
|---|---|
| unset / `full` | the statement, redacted (the default) |
| `shape` | only the shape — `insert into people (name) values ('?')` |
| `off` | nothing; the fingerprint alone |

The fingerprint is computed before any of this, so turning the column down does
not break grouping. The mode comes back with the rows, so a client can say *why*
a statement is blank — a blank with no explanation reads as a bug, and this one
is a setting.

A failure to write an audit row never fails the operation it describes: it is
reported on `daemon.log` at `error` level, because refusing to run something on
the grounds that the record could not be written is worse than a gap in the
record that says loudly that it is a gap.

### Dashboards (`proto >= 8`)

A saved CHART is one statement read as a picture. A DASHBOARD is an arrangement
of several of them on a grid of **twelve columns**, with the dividers and the
words that say what the reader is looking at, and one time range they all answer
to. Twelve because it divides by 2, 3, 4 and 6 — halves, thirds, quarters, and a
sidebar beside a wide one.

| method | params | result |
|---|---|---|
| `store.dashboards.list` | `{ conn }` | `{ dashboards }` — names and times only |
| `store.dashboards.get` | `{ conn, name }` | `{ dashboard, stale, columns }` |
| `store.dashboards.save` | `{ conn, name, dashboard }` | `{ saved, dashboard }` |
| `store.dashboards.remove` | `{ conn, name }` | `{ removed }` |

A tile is `{ kind, x, y, w, h, chart?, text? }`, where `kind` is `chart`,
`divider` or `text`.

**A chart tile references its chart BY NAME.** An index would move the moment a
chart was renamed, removed or re-sorted, and the board would go on drawing
confidently — with the wrong chart under the old heading. A name that no longer
resolves is an error with the name in it, which is a thing somebody can fix.

**What the engine refuses**, and why it is the engine that refuses it: a tile
naming a chart that does not exist on this connection; a tile outside the twelve
columns, or with no width or height; two tiles overlapping; a time range whose
ends are not timestamps, or that runs backwards. A client could check all four.
The point is that it does not have to be trusted to — a board saved by a client
with a bug is a board that is wrong every time it is opened, by every client, for
as long as it exists.

A board that has become invalid since it was saved — usually because a chart it
names was deleted from the other client — still comes back, with `stale` saying
what is wrong. A layout somebody spent time on should be openable and repairable,
not withheld.

#### The shared time range

`{ column, from, to }`. The ends are canonicalised to `YYYY-MM-DD[Thh:mm:ss]`
by a deliberately strict reader, and anything that is not exactly a timestamp is
refused — that is a safety check, not a parsing convenience, because the value is
pasted into a statement by the driver's own template.

Each driver declares `time_filter` in its `DriverMeta`: how THAT engine spells a
time window, over `{col}`, `{from}` and `{to}`. PostgreSQL's is
`{col} >= '{from}' AND {col} < '{to}'`; MongoDB's is a `$gte`/`$lt` document;
Redis's is **empty**, because Redis has no such thing and an empty template is
how a driver says so. Half-open at the top, so two adjacent ranges do not both
contain the row on the boundary.

A chart statement marks where the condition goes with `{{time}}`:

```sql
SELECT who, amount FROM payments WHERE {{time}} ORDER BY amount DESC
```

A statement **without** the marker is not time-filtered, and the board says so on
that tile and counts them in the toolbar — a range that silently applies to four
charts out of six is worse than one that says which two it missed. No client can
know where in somebody's statement a condition belongs (before a `GROUP BY`,
inside a sub-select, nowhere at all), and guessing would put a filter where it
changes what the chart means.

A statement **with** the marker and no range set does **not run**. The first
version took the marker out, which left `… WHERE  ORDER BY …` behind, and the
engine answered `near "ORDER": syntax error` — a terrible way to say "you have
not picked a date". A chart written as `WHERE {{time}}` is a chart about a
window; the honest answer when there is no window is to ask for one. The same
goes for a marker on an engine with no `time_filter`.

### Notifications

| method | params |
|---|---|
| `query.state` | `{ call_id, state, ms, error?, affected? }` — `running` / `done` / `failed` / `cancelled` |
| `daemon.log` | `{ level, message }` |

## The shell suites

```
scripts/build
tests/all.sh              every suite, each under a ceiling, with its exit status
tests/all.sh row dash     only those
```

`tests/all.sh` prints the time each suite took and the status it exited with,
because two of them lied and neither was caught by reading the output.
`profile.sh` printed all 26 of its checks and then hung for ever: its toy servers
had inherited the engine's stdin pipe, so closing it never reached the engine as
EOF. `tunnel.sh` printed `tunnel: PASS` and exited 1, because its cleanup trap ran
`[ -n "$PID" ] && kill "$PID"` under `set -e` against a process that had already
gone — the kill failed, the trap ended early, and its status became the script's.

A suite that hangs reports nothing and stops everything behind it; a suite that
passes and exits non-zero is worse, because the reason reads like a passing test.

## Talking to it by hand

```sh
printf '%s\n' '{"id":1,"method":"rpc.hello","params":{}}' | build/loded
```

`tests/smoke.sh` is the same thing done properly: it spawns `loded`, shakes
hands, connects to a throwaway SQLite database, runs a statement, pages the
result, exercises every `store.*` method and the migration, and checks that each
`conn.test` stage answers about its own layer. It needs no network and no
database server.

`tests/tunnel.sh` covers what only a real SSH server can: it stands up its OWN
sshd — own host key, own `authorized_keys`, own config, own port, all inside a
mktemp directory, touching nothing in `~` or `/etc` — and checks that the
endpoint stage tells the truth THROUGH a tunnel. It skips cleanly when sshd is
not installed.

```sh
scripts/build && tests/smoke.sh && tests/tunnel.sh
```

## The repository

```
.                     the ENGINE — `loded`, the root package
crates/lode-proto     the protocol client: spawn the engine, speak to it
crates/lode-ui        the desktop client — `lode`
```

The engine depends on neither crate, and cannot: a client is a separate process
by design, and the moment the engine knew about one there would be a reason for
the next client to be different.

```sh
scripts/build                  # the engine → build/loded
cargo build --release -p lode-ui    # the desktop client → target/release/lode
```

## The desktop client

`crates/lode-ui` is a client exactly like the editor plugin: it spawns `loded`,
speaks the protocol above, and holds no driver of its own. It opens no socket to
a database and resolves no credential. If it crashes the engine's connections go
with it and nothing else; if the engine crashes the window survives and says so.

What it does: saved connections, the schema tree, a multi-tab query editor, a
virtualised result grid with forward and backward paging and cancel, saved
queries, shared history, and export to CSV / TSV / JSON. **The UI thread never
waits on a database** — every request is sent and collected in a later frame, so
a slow query leaves the rest of the window usable.

### The connection form

Four sections, and between them they are the whole `ConnSpec` the engine takes:

- **Connection** — built from the engine's own `DriverMeta`: the field set, the
  types, the defaults. A new database type in the engine needs no change here.
- **Authentication** — the methods the driver declares (`none`, `password`,
  `client_cert`, `provider`, `kerberos`) with each method's own fields.
- **TLS** — `disable` / `prefer` / `require` / `verify_ca` / `verify_full`, plus
  a CA bundle and a client certificate and key.
- **SSH tunnel** — host, port, user, and `agent` / key file / password.

Each section has a **Test** button wired to `conn.test` with its own stage
(`endpoint`, `auth`, `tls`, `tunnel`). The engine dry-runs that ONE layer, which
is the point of it: a failure names the layer that is wrong instead of handing
back whatever error the deepest layer happened to raise. The answer, success or
failure, is shown verbatim beside the button.

Secrets are typed as **templates** — `{{ env "PGPASSWORD" }}`,
`{{ vault "prod/db" }}`, `{{ cmd "…" }}` — and stored as typed. The client never
resolves one and never sees a resolved value.

### Fonts

The grid and the editor use a monospace face and the chrome a proportional one.
Each is looked for in order: `$LODE_FONT_MONO` / `$LODE_FONT_UI`, then well-known
names under the standard font roots, then a bounded search of those roots. With
nothing found, egui's built-in font is used and the application is plain but
correct. `$LODE_FONT_SIZE` sets the base text size (default 13) and everything
else is derived from it.

Icon glyphs appear only when the monospace face is a **Nerd Font**; otherwise the
buttons carry words. A glyph from a font that is not installed is an empty box,
and an empty box is worse than the word it replaced.

### What it does not do yet

Charts, dashboards, a visual query builder, in-grid row editing, scripting and an
MCP server. They are the next steps, not hidden ones.

### Themes

The client reads TOML theme files — the 48 lvim-colorscheme styles, generated by
that project's `scripts/build lode` into `extras/lode/`:

```sh
mkdir -p ~/.config/lode/themes
cp /path/to/lvim-colorscheme/extras/lode/*.toml ~/.config/lode/themes/
```

Pick one in the Theme menu, or set `LODE_THEME` to a file's stem.
`$LODE_THEME_DIR` overrides the search path. With no theme files at all a
built-in palette applies, so a fresh build is usable immediately.

A theme file is **the editor's own colours**. The generator reads
lvim-colorscheme's resolved highlight groups — `Normal`, `NormalSB`, `Pmenu`,
`NormalFloat`, `StatusLine`, `CursorLine`, `PmenuSel`, `Visual`, `TabLine`,
`Comment`, `Title` and the rest — and copies them unchanged, including the pairs
that measure below WCAG AA. `Normal.fg` on base dark is `#5e8888` at 3.86:1 and
arrives here as `#5e8888`.

That is deliberate and it is this target's whole point: a client that sits beside
the editor all day has to be the same colour as the editor, and a "more readable"
version of a theme is a different theme. The client therefore corrects nothing —
there is not one colour value in its source.

The METRICS are not in the theme file and never will be. Spacing and text sizes
belong to the application: they are the same whichever of the 48 palettes is
loaded, so switching a theme changes no measurement. **Corners are square
everywhere** — buttons, fields, panels, dialogs, dropdowns, tabs — because a
terminal editor has no rounded anything and this client is meant to look like it
belongs next to one.

## Clients

- **`lvim-db`** — the Neovim plugin
  (https://github.com/lvim-tech/lvim-db). Probes `$LODE_DAEMON`, `loded` on
  `$PATH`, then a sibling `lode/build/loded` checkout.
- **`lode`** — the desktop client in `crates/lode-ui`.

## License

BSD-3-Clause. See `LICENSE`.
