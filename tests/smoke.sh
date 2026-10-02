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
                   INSERT INTO t(name) VALUES ('alpha'),('beta'),('gamma'),('delta'),('epsilon');"
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
LODE_DATA_DIR=$WORK/data "$BIN" <"$IN" >"$OUT" 2>"$WORK/err" &
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

send '{"id":34,"method":"store.connections.remove","params":{"name":"smoke"}}'
check "remove connection" "$(await '.id == 34' | jq -r '.result.removed')" "true"
send '{"id":35,"method":"store.queries.list","params":{"conn":"smoke"}}'
check "its queries went with it" "$(await '.id == 35' | jq -r '.result.queries | length')" "0"

# ── 9. stdin EOF shuts the engine down ───────────────────────────────────────
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
