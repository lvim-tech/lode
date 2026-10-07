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
The current protocol is **8**. What each version added is listed in
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
