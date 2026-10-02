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
The current protocol is **2**.

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

## The repository

```
.                     the ENGINE — `loded`, the root package
crates/lode-proto     the protocol client: spawn the engine, speak to it
crates/lode-gui       the desktop client — `lode`
```

The engine depends on neither crate, and cannot: a client is a separate process
by design, and the moment the engine knew about one there would be a reason for
the next client to be different.

```sh
scripts/build                  # the engine → build/loded
cargo build --release -p lode-gui   # the desktop client → target/release/lode
```

## The desktop client

`crates/lode-gui` is a client exactly like the editor plugin: it spawns `loded`,
speaks the protocol above, and holds no driver of its own. It opens no socket to
a database and resolves no credential. If it crashes the engine's connections go
with it and nothing else; if the engine crashes the window survives and says so.

What the first version does: saved connections (a form built from the engine's
own `DriverMeta`, so a new database type needs no change here), the schema tree,
a multi-tab query editor, a virtualised result grid with paging and cancel,
saved queries, shared history, and export to CSV / TSV / JSON. **The UI thread
never waits on a database** — every request is sent and collected in a later
frame, so a slow query leaves the rest of the window usable.

What it does not do yet: charts, dashboards, a visual query builder, in-grid row
editing, a connection test UI, scripting and an MCP server. They are the next
steps, not hidden ones.

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

A theme file is already contrast-grounded by its generator; the client copies it
and corrects nothing, because a client that "improved" a theme would make the 48
themes disagree with the same 48 themes in every other lvim tool.

## Clients

- **`lvim-db`** — the Neovim plugin
  (https://github.com/lvim-tech/lvim-db). Probes `$LODE_DAEMON`, `loded` on
  `$PATH`, then a sibling `lode/build/loded` checkout.
- **`lode`** — the desktop client in `crates/lode-gui`.

## License

BSD-3-Clause. See `LICENSE`.
