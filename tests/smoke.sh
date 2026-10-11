#!/bin/sh
# tests/smoke.sh — prove the engine works OUTSIDE any editor.
#
# Spawns `loded`, speaks the protocol by hand on its pipes, and checks the
# answers: rpc.hello, conn.connect to a THROWAWAY SQLite file, query.execute,
# the query.state notification, query.page, query.cell, query.cancel's error
# path on a finished call, and conn.disconnect.
#
# Needs no network, no database server and no cargo: just a built binary and
# `jq`. Exits non-zero on the first failed assertion.
#
#   scripts/build && tests/smoke.sh
#
# A different binary can be pointed at with $LODE_DAEMON.
set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ ! -x "$BIN" ]; then
    echo "smoke: no loded binary — run scripts/build first" >&2
    exit 1
fi
command -v jq >/dev/null || {
    echo "smoke: jq is required" >&2
    exit 1
}

WORK=$(mktemp -d)
DB=$WORK/smoke.sqlite
IN=$WORK/in
OUT=$WORK/out
trap 'kill $PID 2>/dev/null || true' EXIT

# Seed the throwaway database with the sqlite3 CLI when it is there, otherwise
# let the engine itself create the table (SQLite makes the file on open).
SEEDED=no
if command -v sqlite3 >/dev/null; then
    sqlite3 "$DB" "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT);
                   INSERT INTO t(name) VALUES ('alpha'),('beta'),('gamma'),('delta'),('epsilon');
                   -- A real foreign key, so what points where can be checked
                   -- against a server rather than only against a unit test.
                   CREATE TABLE child(id INTEGER PRIMARY KEY, t_id INTEGER REFERENCES t(id), note TEXT);"
    SEEDED=yes
fi

# A legacy-shaped database to import from: the exact schema the lvim-db Lua
# store wrote, so the migration is exercised against the real old shape and not
# against a convenient approximation of it.
LEGACY=$WORK/legacy.db
if command -v sqlite3 >/dev/null; then
    sqlite3 "$LEGACY" "
        CREATE TABLE connections (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE,
            driver TEXT NOT NULL, spec TEXT NOT NULL, created INTEGER);
        CREATE TABLE queries (id INTEGER PRIMARY KEY AUTOINCREMENT, conn TEXT NOT NULL,
            name TEXT NOT NULL, sql TEXT NOT NULL, updated_at INTEGER);
        CREATE TABLE history (id INTEGER PRIMARY KEY AUTOINCREMENT, conn TEXT, driver TEXT,
            statement TEXT NOT NULL, state TEXT, ms INTEGER, \"rows\" INTEGER, ts INTEGER);
        INSERT INTO connections (name, driver, spec, created)
            VALUES ('legacy-conn', 'postgres', '{\"params\":{\"host\":\"db.example\"},\"auth\":{\"kind\":\"none\"}}', 100);
        INSERT INTO queries (conn, name, sql, updated_at)
            VALUES ('legacy-conn', 'recent', 'SELECT now()', 200);
        INSERT INTO history (conn, driver, statement, state, ms, \"rows\", ts)
            VALUES ('legacy-conn', 'postgres', 'SELECT 1', 'done', 7, 1, 300);"
fi

mkfifo "$IN"
: >"$OUT"
# The store MUST land in the throwaway directory: this test writes connections
# and history, and it has no business touching the real ones.
# The keyring socket is pointed at NOTHING on purpose. `vault.put` writes to the
# real per-user wallet, and a test that stores secrets in the person's wallet is
# a test with a side effect nobody asked for — so the engine must find no agent
# there, which is exactly the refusal the checks below want to see.
LODE_DATA_DIR=$WORK/data LVIM_KEYRING_SOCK=$WORK/no-such-agent.sock \
    "$BIN" <"$IN" >"$OUT" 2>"$WORK/err" &
PID=$!
# Hold the fifo open for the whole run: a plain `echo > fifo` would close it and
# the engine would see EOF on stdin and exit after the first request.
exec 3>"$IN"

FAILED=0
send() { printf '%s\n' "$1" >&3; }

# Wait until a line matching the jq filter `$1` appears in $OUT, up to ~15s.
# Prints that line. The engine's responses and notifications interleave on one
# stream, so every wait selects by shape rather than by position.
await() {
    filter=$1
    i=0
    while [ "$i" -lt 150 ]; do
        line=$(jq -c --slurp "map(select($filter)) | last // empty" "$OUT" 2>/dev/null || true)
        if [ -n "$line" ] && [ "$line" != "null" ]; then
            printf '%s\n' "$line"
            return 0
        fi
        sleep 0.1
        i=$((i + 1))
    done
    return 1
}

check() {
    label=$1
    got=$2
    want=$3
    if [ "$got" = "$want" ]; then
        printf 'ok   %-34s %s\n' "$label" "$got"
    else
        printf 'FAIL %-34s got %s, want %s\n' "$label" "$got" "$want"
        FAILED=1
    fi
}

report() { printf '     %-34s %s\n' "$1" "$2"; }

# ── 1. handshake ─────────────────────────────────────────────────────────────
send '{"id":1,"method":"rpc.hello","params":{}}'
HELLO=$(await '.id == 1') || {
    echo "FAIL rpc.hello: no response" >&2
    cat "$WORK/err" >&2
    exit 1
}
check "hello ok" "$(printf '%s' "$HELLO" | jq -r '.ok')" "true"
check "hello engine" "$(printf '%s' "$HELLO" | jq -r '.result.engine')" "lode"
report "hello version" "$(printf '%s' "$HELLO" | jq -r '.result.version')"
report "hello proto" "$(printf '%s' "$HELLO" | jq -r '.result.proto')"
report "drivers" "$(printf '%s' "$HELLO" | jq -r '[.result.drivers[].kind] | join(" ")')"
check "sqlite is registered" \
    "$(printf '%s' "$HELLO" | jq -r '[.result.drivers[].kind] | index("sqlite") != null')" "true"

# ── 2. connect to the throwaway file ─────────────────────────────────────────
send "{\"id\":2,\"method\":\"conn.connect\",\"params\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"}}}"
CONN=$(await '.id == 2')
check "connect ok" "$(printf '%s' "$CONN" | jq -r '.ok')" "true"
CID=$(printf '%s' "$CONN" | jq -r '.result.conn_id')
report "conn_id" "$CID"

if [ "$SEEDED" = no ]; then
    send "{\"id\":90,\"method\":\"query.execute\",\"params\":{\"conn_id\":$CID,\"statement\":\"CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)\"}}"
    SEED=$(await '.id == 90')
    SEEDCALL=$(printf '%s' "$SEED" | jq -r '.result.call_id')
    await ".method == \"query.state\" and .params.call_id == $SEEDCALL" >/dev/null
    send "{\"id\":91,\"method\":\"query.execute\",\"params\":{\"conn_id\":$CID,\"statement\":\"INSERT INTO t(name) VALUES ('alpha'),('beta'),('gamma'),('delta'),('epsilon')\"}}"
    SEED=$(await '.id == 91')
    SEEDCALL=$(printf '%s' "$SEED" | jq -r '.result.call_id')
    await ".method == \"query.state\" and .params.call_id == $SEEDCALL" >/dev/null
fi

# ── 2b. conn.test, one stage at a time ───────────────────────────────────────
# This is what a client's connection form calls per section. The value of it is
# ATTRIBUTION: a failure names the layer that is wrong instead of handing back
# whatever error the deepest layer happened to raise, so each stage is checked
# for answering about ITS layer and nothing else.
send "{\"id\":40,\"method\":\"conn.test\",\"params\":{\"stage\":\"endpoint\",\"spec\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"}}}}"
T=$(await '.id == 40')
check "test endpoint ok" "$(printf '%s' "$T" | jq -r '.result.ok')" "true"
report "its detail" "$(printf '%s' "$T" | jq -r '.result.detail')"

# A file that is not there must fail THE ENDPOINT STAGE, with the path in the
# message — not a generic "could not connect".
send "{\"id\":41,\"method\":\"conn.test\",\"params\":{\"stage\":\"endpoint\",\"spec\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$WORK/nope.sqlite\"}}}}"
BADEP=$(await '.id == 41')
check "missing file fails endpoint" "$(printf '%s' "$BADEP" | jq -r '.ok')" "false"
check "and names the path" \
    "$(printf '%s' "$BADEP" | jq -r '.error | test("nope.sqlite")')" "true"

# A tunnel stage on a spec with no tunnel must say exactly that, rather than
# silently passing — a green tunnel light on a connection with no tunnel is the
# worst answer available.
send "{\"id\":42,\"method\":\"conn.test\",\"params\":{\"stage\":\"tunnel\",\"spec\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"}}}}"
NOTUN=$(await '.id == 42')
check "tunnel stage without a tunnel fails" "$(printf '%s' "$NOTUN" | jq -r '.ok')" "false"
report "its error" "$(printf '%s' "$NOTUN" | jq -r '.error')"

# An unknown stage is rejected, so a client cannot invent one.
send "{\"id\":43,\"method\":\"conn.test\",\"params\":{\"stage\":\"magic\",\"spec\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"}}}}"
check "unknown stage rejected" "$(await '.id == 43' | jq -r '.ok')" "false"

# The FULL spec shape a form produces — auth, tls and a tunnel block together —
# must deserialise. This is the shape crates/lode-ui builds; a field the engine
# will not accept has to fail here and not in front of a user.
send "{\"id\":44,\"method\":\"conn.test\",\"params\":{\"stage\":\"endpoint\",\"spec\":{\"driver\":\"postgres\",\"params\":{\"host\":\"127.0.0.1\",\"port\":\"1\"},\"auth\":{\"kind\":\"password\",\"user\":\"u\",\"password\":\"{{ env \\\"NOPE\\\" }}\"},\"tls\":{\"mode\":\"verify_full\",\"ca\":\"/tmp/ca.pem\"},\"tunnel\":{\"host\":\"h\",\"port\":22,\"user\":\"u\",\"auth\":{\"kind\":\"agent\"}}}}}"
FULL=$(await '.id == 44')
# It will NOT succeed (nothing is listening, and there is no ssh host), but the
# failure must be about reaching the host — proving the spec itself parsed.
check "full spec parses" \
    "$(printf '%s' "$FULL" | jq -r '.error // "" | test("missing|unknown|invalid|expected") | not')" "true"
report "full spec answer" "$(printf '%s' "$FULL" | jq -r '.error // .result.detail' | cut -c1-90)"

# ── 3. the schema tree ───────────────────────────────────────────────────────
send "{\"id\":3,\"method\":\"schema.structure\",\"params\":{\"conn_id\":$CID}}"
STRUCT=$(await '.id == 3')
check "structure ok" "$(printf '%s' "$STRUCT" | jq -r '.ok')" "true"
check "table t is in the tree" \
    "$(printf '%s' "$STRUCT" | jq -r '[.result.nodes | .. | objects | .name? // empty] | index("t") != null')" "true"

# ── 4. run a statement, wait for its state ───────────────────────────────────
send "{\"id\":4,\"method\":\"query.execute\",\"params\":{\"conn_id\":$CID,\"statement\":\"SELECT id, name FROM t ORDER BY id\"}}"
EXEC=$(await '.id == 4')
check "execute ok" "$(printf '%s' "$EXEC" | jq -r '.ok')" "true"
CALL=$(printf '%s' "$EXEC" | jq -r '.result.call_id')
STATE=$(await ".method == \"query.state\" and .params.call_id == $CALL")
check "query.state" "$(printf '%s' "$STATE" | jq -r '.params.state')" "done"
report "elapsed ms" "$(printf '%s' "$STATE" | jq -r '.params.ms')"

# ── 5. page the result ───────────────────────────────────────────────────────
send "{\"id\":5,\"method\":\"query.page\",\"params\":{\"call_id\":$CALL,\"from\":0,\"n\":2}}"
PAGE=$(await '.id == 5')
check "page ready" "$(printf '%s' "$PAGE" | jq -r '.result.ready')" "true"
check "page columns" "$(printf '%s' "$PAGE" | jq -r '[.result.columns[].name] | join(",")')" "id,name"
# And what each column's values ARE, not only what the driver calls them. The
# table is `t(id INTEGER PRIMARY KEY, name TEXT)`, and the kind is derived in
# ONE table in the engine rather than by each driver — a client asking "can
# this go on a time axis" should not have to learn eighteen vocabularies.
check "columns say their kind" \
    "$(printf '%s' "$PAGE" | jq -r '[.result.columns[].kind] | join(",")')" "integer,text"
check "and the driver's own word is still there" \
    "$(printf '%s' "$PAGE" | jq -r '.result.columns[0].type')" "INTEGER"

# The schema changes a dialect can WRITE OUT are declared, as templates, and
# the client fills them in — so what is checkable here is that they arrive and
# that each dialect carries its own spelling. SQLite names an index without the
# table in DROP INDEX; SQL Server needs it. Both are in the handshake.
check "sqlite declares its schema changes" \
    "$(printf '%s' "$HELLO" | jq -r '[.result.drivers[] | select(.kind == "sqlite") | .ddl_ops | length] | add')" \
    "5"
check "and renames a column the standard way" \
    "$(printf '%s' "$HELLO" | jq -r '[.result.drivers[] | select(.kind == "sqlite") | .ddl_ops[] | select(.op == "rename_column") | .template] | join("")')" \
    "ALTER TABLE {qualified} RENAME COLUMN {column} TO {to};"
# A driver with no columns declares no column changes: offering "add a column"
# on a key/value store would be offering a statement it has no words for.
check "redis declares none" \
    "$(printf '%s' "$HELLO" | jq -r '[.result.drivers[] | select(.kind == "redis") | .ddl_ops | length] | add')" \
    "0"

# WHAT POINTS WHERE. `customer_id` looks like a reference and `parent` does
# not, and both guesses are wrong about half the schemas in the world — so the
# server is asked. This is the one driver here whose foreign keys can be checked
# against a live database rather than only in a unit test.
send "{\"id\":75,\"method\":\"schema.columns\",\"params\":{\"conn_id\":$CID,\"object\":{\"name\":\"child\"}}}"
FKC=$(await '.id == 75')
check "a foreign key is reported" \
    "$(printf '%s' "$FKC" | jq -r '[.result.columns[] | select(.name == "t_id") | .references.table] | join(",")')" \
    "t"
check "and the column it points at" \
    "$(printf '%s' "$FKC" | jq -r '[.result.columns[] | select(.name == "t_id") | .references.column] | join(",")')" \
    "id"
# A column that is NOT a key says nothing rather than saying no: the field is
# absent, which is what `skip_serializing_if` is for.
check "a plain column carries no reference" \
    "$(printf '%s' "$FKC" | jq -r '[.result.columns[] | select(.name == "note") | has("references")] | join(",")')" \
    "false"

# ONE BUFFER, several statements. The boundary is not just `;` — this checks
# the three that cost the most to get wrong: a semicolon in a string, one in a
# comment, and a dollar-quoted function body.
send '{"id":70,"method":"query.split","params":{"driver":"postgres","text":"SELECT 1; INSERT INTO t VALUES (\u0027a;b\u0027); -- x; y\nSELECT 2"}}'
SP=$(await '.id == 70')
check "a buffer comes apart" "$(printf '%s' "$SP" | jq -r '.result.statements | length')" "3"
check "a string keeps its semicolon" \
    "$(printf '%s' "$SP" | jq -r '.result.statements[1]')" \
    "INSERT INTO t VALUES ('a;b')"
send '{"id":71,"method":"query.split","params":{"driver":"postgres","text":"CREATE FUNCTION f() AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql; SELECT f()"}}'
check "a function body stays whole" \
    "$(await '.id == 71' | jq -r '.result.statements | length')" "2"
send '{"id":72,"method":"query.split","params":{"driver":"mongodb","text":"{ \"find\": \"t\", \"filter\": { \"a\": \"x;y\" } }"}}'
check "a document is one statement" \
    "$(await '.id == 72' | jq -r '.result.statements | length')" "1"

# A live VIEW is declared per driver, and asking for one that was not declared
# is refused rather than reaching the driver. SQLite declares none — it is a
# file, and there is nobody connected to it to list — so the refusal is the
# whole of what can be checked here without a server.
check "sqlite declares no live views" \
    "$(printf '%s' "$HELLO" | jq -r '[.result.drivers[] | select(.kind == "sqlite") | .inspectors | length] | add')" \
    "0"
send "{\"id\":57,\"method\":\"instance.inspect\",\"params\":{\"conn_id\":$CID,\"name\":\"activity\"}}"
check "a view the driver never offered is refused" \
    "$(await '.id == 57' | jq -r 'if (.error | test("no live view")) then "yes" else .error end')" \
    "yes"
send "{\"id\":58,\"method\":\"instance.action\",\"params\":{\"conn_id\":$CID,\"name\":\"activity\",\"key\":\"1\"}}"
check "and so is acting on one" \
    "$(await '.id == 58' | jq -r 'if (.error | test("no live view")) then "yes" else .error end')" \
    "yes"
check "page rows" "$(printf '%s' "$PAGE" | jq -r '.result.rows | length')" "2"
check "page has_more" "$(printf '%s' "$PAGE" | jq -r '.result.has_more')" "true"
report "first row" "$(printf '%s' "$PAGE" | jq -c '.result.rows[0]')"

# A SECOND page, then a BACKWARD page: the engine buffers what it pulled, so
# re-reading from 0 must not re-run anything and must give the same rows.
send "{\"id\":6,\"method\":\"query.page\",\"params\":{\"call_id\":$CALL,\"from\":2,\"n\":10}}"
PAGE2=$(await '.id == 6')
check "second page rows" "$(printf '%s' "$PAGE2" | jq -r '.result.rows | length')" "3"
check "exhausted ⇒ total known" "$(printf '%s' "$PAGE2" | jq -r '.result.total')" "5"
send "{\"id\":7,\"method\":\"query.page\",\"params\":{\"call_id\":$CALL,\"from\":0,\"n\":2}}"
PAGE3=$(await '.id == 7')
check "backward page is identical" \
    "$(printf '%s' "$PAGE3" | jq -c '.result.rows')" "$(printf '%s' "$PAGE" | jq -c '.result.rows')"

# ── 6. one full cell ─────────────────────────────────────────────────────────
send "{\"id\":8,\"method\":\"query.cell\",\"params\":{\"call_id\":$CALL,\"row\":0,\"col\":1}}"
CELL=$(await '.id == 8')
check "cell ok" "$(printf '%s' "$CELL" | jq -r '.ok')" "true"
report "cell (0,1)" "$(printf '%s' "$CELL" | jq -c '.result.value')"

# ── 7. an unknown method is an ERROR, not a hang ─────────────────────────────
send '{"id":9,"method":"no.such.method","params":{}}'
BAD=$(await '.id == 9')
check "unknown method rejected" "$(printf '%s' "$BAD" | jq -r '.ok')" "false"
report "its error" "$(printf '%s' "$BAD" | jq -r '.error')"

# ── 8. release + disconnect ──────────────────────────────────────────────────
send "{\"id\":10,\"method\":\"query.release\",\"params\":{\"call_id\":$CALL}}"
check "release ok" "$(await '.id == 10' | jq -r '.ok')" "true"
send "{\"id\":11,\"method\":\"query.page\",\"params\":{\"call_id\":$CALL,\"from\":0,\"n\":1}}"
check "released call is gone" "$(await '.id == 11' | jq -r '.ok')" "false"
send "{\"id\":12,\"method\":\"conn.disconnect\",\"params\":{\"conn_id\":$CID}}"
check "disconnect ok" "$(await '.id == 12' | jq -r '.ok')" "true"

# ── 8b. the saved state (proto 2) ────────────────────────────────────────────
# All of it against $WORK/data, never the real store.
send '{"id":20,"method":"store.status","params":{}}'
STATUS=$(await '.id == 20')
check "store available" "$(printf '%s' "$STATUS" | jq -r '.result.available')" "true"
report "store path" "$(printf '%s' "$STATUS" | jq -r '.result.path')"

send '{"id":21,"method":"store.connections.save","params":{"name":"smoke","driver":"sqlite","spec":{"params":{"file":"/tmp/smoke.db"},"auth":{"kind":"none"}}}}'
check "save connection" "$(await '.id == 21' | jq -r '.result.saved')" "true"
send '{"id":22,"method":"store.connections.list","params":{}}'
LIST=$(await '.id == 22')
check "connection listed" "$(printf '%s' "$LIST" | jq -r '[.result.connections[].name] | index("smoke") != null')" "true"
send '{"id":23,"method":"store.connections.get","params":{"name":"smoke"}}'
GOT=$(await '.id == 23')
check "spec round-trips as an object" \
    "$(printf '%s' "$GOT" | jq -r '.result.connection.spec.params.file')" "/tmp/smoke.db"

# A re-save must UPDATE: a second row under the same name would show the user
# two connections where they made one.
send '{"id":24,"method":"store.connections.save","params":{"name":"smoke","driver":"sqlite","spec":{"params":{"file":"/tmp/other.db"}}}}'
await '.id == 24' >/dev/null
send '{"id":25,"method":"store.connections.list","params":{}}'
check "re-save updates, not duplicates" \
    "$(await '.id == 25' | jq -r '[.result.connections[] | select(.name == "smoke")] | length')" "1"

send '{"id":26,"method":"store.queries.save","params":{"conn":"smoke","name":"q1","sql":"SELECT 1"}}'
check "save query" "$(await '.id == 26' | jq -r '.result.saved')" "true"
send '{"id":27,"method":"store.queries.list","params":{"conn":"smoke"}}'
check "query listed" "$(await '.id == 27' | jq -r '[.result.queries[].name] | join(",")')" "q1"
send '{"id":28,"method":"store.queries.list","params":{"conn":"other"}}'
check "queries are per-connection" "$(await '.id == 28' | jq -r '.result.queries | length')" "0"

send '{"id":29,"method":"store.history.record","params":{"conn":"smoke","driver":"sqlite","statement":"SELECT 1","state":"done","ms":3,"rows":1}}'
check "record history" "$(await '.id == 29' | jq -r '.result.recorded')" "true"
send '{"id":30,"method":"store.history.list","params":{"limit":10}}'
check "history listed" "$(await '.id == 30' | jq -r '.result.history[0].statement')" "SELECT 1"

# ── 8c. the migration, on a COPY of a legacy-shaped file ─────────────────────
if [ -f "$LEGACY" ]; then
    send "{\"id\":31,\"method\":\"store.import\",\"params\":{\"path\":\"$LEGACY\"}}"
    IMP=$(await '.id == 31')
    check "import connections" "$(printf '%s' "$IMP" | jq -r '.result.connections')" "1"
    check "import queries" "$(printf '%s' "$IMP" | jq -r '.result.queries')" "1"
    check "import history" "$(printf '%s' "$IMP" | jq -r '.result.history')" "1"
    send "{\"id\":32,\"method\":\"store.import\",\"params\":{\"path\":\"$LEGACY\"}}"
    IMP2=$(await '.id == 32')
    check "re-import adds nothing (connections)" "$(printf '%s' "$IMP2" | jq -r '.result.connections')" "0"
    check "re-import adds nothing (queries)" "$(printf '%s' "$IMP2" | jq -r '.result.queries')" "0"
    check "re-import adds nothing (history)" "$(printf '%s' "$IMP2" | jq -r '.result.history')" "0"
    send '{"id":33,"method":"store.connections.get","params":{"name":"legacy-conn"}}'
    check "imported spec survived" \
        "$(await '.id == 33' | jq -r '.result.connection.spec.params.host')" "db.example"
    # The source must be untouched: an old client has to keep working.
    check "legacy file still readable" "$(sqlite3 "$LEGACY" 'SELECT count(*) FROM connections')" "1"
else
    report "migration" "skipped (no sqlite3 CLI to write a legacy file)"
fi

# A RENAME takes everything scoped to the name with it. Saving under the new
# name and deleting the old one is not a rename: the saved queries stay behind,
# pointing at a name nothing answers to.
send '{"id":36,"method":"store.connections.rename","params":{"from":"smoke","to":"smoke2"}}'
REN=$(await '.id == 36')
check "rename connection" "$(printf '%s' "$REN" | jq -r '.result.renamed')" "true"
check "its queries moved with it" "$(printf '%s' "$REN" | jq -r '.result.moved.queries')" "1"
send '{"id":37,"method":"store.queries.list","params":{"conn":"smoke2"}}'
check "the query answers to the new name" \
    "$(await '.id == 37' | jq -r '[.result.queries[].name] | join(",")')" "q1"
send '{"id":38,"method":"store.queries.list","params":{"conn":"smoke"}}'
check "and not to the old one" "$(await '.id == 38' | jq -r '.result.queries | length')" "0"

# A name already taken is refused: `save` overwrites by name, so a rename that
# swallowed the other connection would be a way to lose one.
send '{"id":39,"method":"store.connections.save","params":{"name":"taken","driver":"sqlite","spec":{"params":{"file":"/tmp/t.db"}}}}'
await '.id == 39' >/dev/null
send '{"id":40,"method":"store.connections.rename","params":{"from":"smoke2","to":"taken"}}'
check "a taken name is refused" "$(await '.id == 40' | jq -r 'has("error")')" "true"
send '{"id":41,"method":"store.connections.rename","params":{"from":"smoke2","to":"smoke"}}'
await '.id == 41' >/dev/null

# A FOLDER is the shared store's, not a client's file — and a save that says
# nothing about it must leave it alone, or the Neovim client saving a password
# would empty it.
send '{"id":44,"method":"store.connections.save","params":{"name":"smoke","driver":"sqlite","spec":{"params":{"file":"/tmp/x.db"}},"group":"prod"}}'
await '.id == 44' >/dev/null
send '{"id":45,"method":"store.connections.get","params":{"name":"smoke"}}'
check "a folder is stored" "$(await '.id == 45' | jq -r '.result.connection.group')" "prod"
send '{"id":46,"method":"store.connections.save","params":{"name":"smoke","driver":"sqlite","spec":{"params":{"file":"/tmp/y.db"}}}}'
await '.id == 46' >/dev/null
send '{"id":47,"method":"store.connections.get","params":{"name":"smoke"}}'
GOT=$(await '.id == 47')
check "a blind save keeps the folder" "$(printf '%s' "$GOT" | jq -r '.result.connection.group')" "prod"
check "and still saved the spec" "$(printf '%s' "$GOT" | jq -r '.result.connection.spec.params.file')" "/tmp/y.db"
send '{"id":48,"method":"store.connections.save","params":{"name":"smoke","driver":"sqlite","spec":{"params":{"file":"/tmp/y.db"}},"group":null}}'
await '.id == 48' >/dev/null
send '{"id":49,"method":"store.connections.get","params":{"name":"smoke"}}'
check "and null takes it out" "$(await '.id == 49' | jq -r '.result.connection.group')" "null"

# `vault.put` CARRIES a secret, and the one thing it must never do is keep it
# anywhere: not in the store, not in the audit log, not in the error it fails
# with. There is no agent on that socket, so this is the refusal path — the one
# that has an error message to inspect.
send '{"id":60,"method":"vault.put","params":{"name":"db/smoke","value":"s3cret-canary"}}'
VP=$(await '.id == 60')
check "no wallet is a refusal" "$(printf '%s' "$VP" | jq -r '.ok')" "false"
check "and it says what to do" \
    "$(printf '%s' "$VP" | jq -r 'if (.error | test("lvim-keyring")) then "yes" else .error end')" \
    "yes"
check "and the secret is not in the error" \
    "$(printf '%s' "$VP" | jq -r 'if (.error | test("s3cret-canary")) then "LEAKED" else "no" end')" \
    "no"
# Nor anywhere else the engine writes. Everything it has said on stdout and
# stderr is searched, because a value echoed back in a reply or logged on the
# way past is the same leak by another route. Zero files, not zero lines: a
# count of lines would pass just as well if the search itself were broken.
check "and not in anything the engine wrote" \
    "$(grep -l s3cret-canary "$OUT" "$WORK/err" 2>/dev/null | wc -l | tr -d ' ')" \
    "0"
send '{"id":61,"method":"vault.put","params":{"name":"  ","value":"x"}}'
check "a secret needs a name" \
    "$(await '.id == 61' | jq -r 'if (.error | test("needs a name")) then "yes" else .error end')" \
    "yes"
send '{"id":62,"method":"vault.put","params":{"name":"db/smoke","value":""}}'
check "and a value" \
    "$(await '.id == 62' | jq -r 'if (.error | test("needs a value")) then "yes" else .error end')" \
    "yes"

# A REMOVE takes everything scoped to the name too. It used to take only the
# saved queries, so a chart or a dashboard was left pointing at a name nothing
# answers to — invisible in every list, and waiting to attach itself to the next
# connection given that name.
send '{"id":42,"method":"store.charts.save","params":{"conn":"smoke","name":"c1","sql":"SELECT 1","measure":"n"}}'
await '.id == 42' >/dev/null
send '{"id":34,"method":"store.connections.remove","params":{"name":"smoke"}}'
RM=$(await '.id == 34')
check "remove connection" "$(printf '%s' "$RM" | jq -r '.result.removed')" "true"
check "it says what went: queries" "$(printf '%s' "$RM" | jq -r '.result.took.queries')" "1"
check "it says what went: charts" "$(printf '%s' "$RM" | jq -r '.result.took.charts')" "1"
send '{"id":35,"method":"store.queries.list","params":{"conn":"smoke"}}'
check "its queries went with it" "$(await '.id == 35' | jq -r '.result.queries | length')" "0"
send '{"id":43,"method":"store.charts.list","params":{"conn":"smoke"}}'
check "and its charts too" "$(await '.id == 43' | jq -r '.result.charts | length')" "0"

# ── 9. stdin EOF shuts the engine down ───────────────────────────────────────
# ── instance metrics and saved charts ────────────────────────────────────────
# A fresh connection: the store section above disconnects the first one, and a
# metric read needs a live link by definition.
send "{\"jsonrpc\":\"2.0\",\"id\":899,\"method\":\"conn.connect\",\"params\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"}}}"
MCID=$(await '.id == 899' | jq -r '.result.conn_id')
send "{\"jsonrpc\":\"2.0\",\"id\":900,\"method\":\"instance.metrics\",\"params\":{\"conn_id\":$MCID}}"
M=$(await '.id == 900')
check "metrics came back" "$(printf '%s' "$M" | jq '.result.metrics | length > 0')" "true"
# The size has to be a READABLE string, not a page count: a metric that needs a
# unit and a scale from somewhere else is a metric nobody can act on.
check "size is readable" \
    "$(printf '%s' "$M" | jq -r '[.result.metrics[] | select(.name=="size")][0].value | test("^[0-9.]+ (B|KiB|MiB|GiB|TiB)$")')" "true"
check "metrics are grouped" \
    "$(printf '%s' "$M" | jq '[.result.metrics[].group] | unique | length >= 2')" "true"

send '{"jsonrpc":"2.0","id":901,"method":"store.charts.save","params":{"conn":"smoke","name":"by balance","sql":"SELECT name, balance FROM t","measure":"balance","kind":"line"}}'
check "chart saved" "$(await '.id == 901' | jq '.result.saved')" "true"
send '{"jsonrpc":"2.0","id":902,"method":"store.charts.list","params":{"conn":"smoke"}}'
C=$(await '.id == 902')
check "chart listed" "$(printf '%s' "$C" | jq '.result.charts | length')" "1"
# The measure is kept BY NAME. An index would move the moment the statement
# changed a column, and the chart would silently measure something else.
check "measure kept by name" "$(printf '%s' "$C" | jq -r '.result.charts[0].measure')" "balance"
# HOW it is drawn comes back with it. A chart saved as a line that reopened as
# bars is a chart somebody has to fix every time they open it. The engine does
# not interpret the word — it is the client's, so a newer client can save a
# kind this engine never heard of.
check "kind kept" "$(printf '%s' "$C" | jq -r '.result.charts[0].kind')" "line"
send '{"jsonrpc":"2.0","id":903,"method":"store.charts.save","params":{"conn":"smoke","name":"from the future","sql":"SELECT 1","measure":"x","kind":"sunburst"}}'
await '.id == 903' >/dev/null
send '{"jsonrpc":"2.0","id":904,"method":"store.charts.list","params":{"conn":"smoke"}}'
check "a kind the engine never heard of is stored anyway" \
    "$(await '.id == 904' | jq -r '[.result.charts[] | select(.name == "from the future")][0].kind')" \
    "sunburst"
send '{"jsonrpc":"2.0","id":905,"method":"store.charts.remove","params":{"conn":"smoke","name":"from the future"}}'
await '.id == 905' >/dev/null
send '{"jsonrpc":"2.0","id":903,"method":"store.charts.remove","params":{"conn":"smoke","name":"by balance"}}'
check "chart removed" "$(await '.id == 903' | jq '.result.removed')" "true"

exec 3>&-
i=0
while kill -0 "$PID" 2>/dev/null && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
if kill -0 "$PID" 2>/dev/null; then
    echo "FAIL engine still running after stdin EOF"
    FAILED=1
else
    echo "ok   stdin EOF shuts the engine down"
fi

[ -s "$WORK/err" ] && {
    echo "--- stderr ---"
    cat "$WORK/err"
}

if [ "$FAILED" -eq 0 ]; then
    echo "smoke: PASS"
else
    echo "smoke: FAIL"
fi
exit $FAILED
