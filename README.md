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
  [`lvim-db`](https://github.com/lvim-tech/lvim-db) Neovim plugin and (next) a
  desktop GUI.
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
The current protocol is **1**.

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
caused it: `endpoint` (stat the file, or dial the socket — through the tunnel
when one is configured), `tunnel` (stand the SSH forward up alone), `tls` and
`auth` (a real connect, closed again).

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

### Notifications

| method | params |
|---|---|
| `query.state` | `{ call_id, state, ms, error?, affected? }` — `running` / `done` / `failed` / `cancelled` |
| `daemon.log` | `{ level, message }` |

## Talking to it by hand

```sh
printf '%s\n' '{"id":1,"method":"rpc.hello","params":{}}' | build/loded
```

`tests/smoke.sh` is the same thing done properly: it spawns `loded`, shakes
hands, connects to a throwaway SQLite database, runs a statement, pages the
result and disconnects. It needs no network and no database server.

```sh
scripts/build && tests/smoke.sh
```

## Clients

- **`lvim-db`** — the Neovim plugin.

## License

BSD-3-Clause. See `LICENSE`.
