#!/bin/sh
# tests/row.sh — prove that editing a row refuses everything it should.
#
# This is the test for the ONE feature in lode that writes into somebody else's
# data. What it checks is mostly REFUSALS, because that is where the value is: a
# row editor that works is ordinary, and a row editor that cannot be talked into
# writing the wrong row is the point.
#
#   a table with no primary key                     refused, and says why
#   a key that is not exactly the primary key       refused
#   a change to a key column                        refused
#   no `confirm`                                    returns the statement, writes NOTHING
#   a key that matches nothing                      refused, and the table is untouched
#   a driver that does not claim `edit`             refused
#   a value that looks like SQL                     stored as a VALUE
#
# Needs no network and no database server: a throwaway SQLite file and `jq`.
#
#   scripts/build && tests/row.sh
set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ ! -x "$BIN" ]; then
    echo "row: no loded binary — run scripts/build first" >&2
    exit 1
fi
command -v jq >/dev/null || { echo "row: jq is required" >&2; exit 1; }
command -v sqlite3 >/dev/null || { echo "row: sqlite3 is required" >&2; exit 1; }

WORK=$(mktemp -d)
DB=$WORK/row.sqlite
IN=$WORK/in
OUT=$WORK/out
FAILED=0
trap 'kill $PID 2>/dev/null || true' EXIT

sqlite3 "$DB" "
    CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT, note TEXT);
    INSERT INTO people(id, name, note) VALUES (1,'Ada','first'),(2,'Grace','second'),(3,'Alan','third');
    -- No key at all. A row in here cannot be addressed, and that is the point.
    CREATE TABLE loose(a TEXT, b TEXT);
    INSERT INTO loose VALUES ('x','y'),('x','z');
    -- A key of two columns, to prove 'exactly the key' means both.
    CREATE TABLE pair(l TEXT, r TEXT, v TEXT, PRIMARY KEY (l, r));
    INSERT INTO pair VALUES ('a','b','one'),('a','c','two');
"

mkfifo "$IN"
: > "$OUT"
# LODE_DATA_DIR in the throwaway directory, ALWAYS: this test makes the engine
# write audit rows, and an audit row from a test belongs in a test's database.
LODE_DATA_DIR=$WORK/data "$BIN" < "$IN" > "$OUT" 2>"$WORK/err" &
PID=$!
exec 3>"$IN"

send() { printf '%s\n' "$1" >&3; }

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
    echo "TIMED OUT waiting for $filter" >&2
    return 1
}

check() {
    label=$1; got=$2; want=$3
    if [ "$got" = "$want" ]; then
        printf 'ok   %-42s %s\n' "$label" "$got"
    else
        printf 'FAIL %-42s got %s, want %s\n' "$label" "$got" "$want"
        FAILED=1
    fi
}

# An error whose MESSAGE has to mention something: a refusal that does not say
# why is a refusal somebody will work around instead of understanding.
mentions() {
    label=$1; line=$2; word=$3
    msg=$(printf '%s' "$line" | jq -r '.error // ""')
    case "$msg" in
        *"$word"*) printf 'ok   %-42s %s\n' "$label" "says \"$word\"" ;;
        *) printf 'FAIL %-42s %s\n' "$label" "message was: $msg"; FAILED=1 ;;
    esac
}

rows_of() { sqlite3 "$DB" "$1"; }

send '{"jsonrpc":"2.0","id":1,"method":"rpc.hello"}'
HELLO=$(await '.id == 1')
check "protocol carries row.write" "$(printf '%s' "$HELLO" | jq '.result.proto >= 5')" "true"
check "sqlite claims edit" \
    "$(printf '%s' "$HELLO" | jq '[.result.drivers[] | select(.kind=="sqlite")][0].caps.edit')" "true"
check "duckdb does not claim edit" \
    "$(printf '%s' "$HELLO" | jq '[.result.drivers[] | select(.kind=="duckdb")][0].caps.edit')" "false"

send "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"conn.connect\",\"params\":{\"driver\":\"sqlite\",\"name\":\"rowtest\",\"params\":{\"file\":\"$DB\"},\"auth\":{\"kind\":\"none\"}}}"
CID=$(await '.id == 2' | jq '.result.conn_id')
check "connected" "$([ -n "$CID" ] && echo yes)" "yes"

# ── a table with no key ──────────────────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"row.write\",\"params\":{\"conn_id\":$CID,\"obj\":{\"name\":\"loose\",\"schema\":\"main\"},\"key\":[[\"a\",\"x\"]],\"set\":[[\"b\",\"zz\"]],\"confirm\":true}}"
L=$(await '.id == 10')
mentions "no primary key, refused" "$L" "no primary key"
check "loose untouched" "$(rows_of "SELECT count(*) FROM loose WHERE b='zz'")" "0"

# ── a key that is only half the key ──────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"row.write\",\"params\":{\"conn_id\":$CID,\"obj\":{\"name\":\"pair\",\"schema\":\"main\"},\"key\":[[\"l\",\"a\"]],\"set\":[[\"v\",\"changed\"]],\"confirm\":true}}"
L=$(await '.id == 11')
mentions "half a composite key, refused" "$L" "must be exactly"
check "pair untouched" "$(rows_of "SELECT count(*) FROM pair WHERE v='changed'")" "0"

# ── changing a key column ────────────────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"row.write\",\"params\":{\"conn_id\":$CID,\"obj\":{\"name\":\"people\",\"schema\":\"main\"},\"key\":[[\"id\",1]],\"set\":[[\"id\",99]],\"confirm\":true}}"
L=$(await '.id == 12')
mentions "changing the key, refused" "$L" "moves the row"
check "id 1 still there" "$(rows_of "SELECT count(*) FROM people WHERE id=1")" "1"

# ── without confirm ──────────────────────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"row.write\",\"params\":{\"conn_id\":$CID,\"obj\":{\"name\":\"people\",\"schema\":\"main\"},\"key\":[[\"id\",1]],\"set\":[[\"name\",\"Ada L\"]]}}"
L=$(await '.id == 13')
check "unconfirmed: applied false" "$(printf '%s' "$L" | jq '.result.applied')" "false"
check "unconfirmed: shows the statement" \
    "$(printf '%s' "$L" | jq -r '.result.statement | test("UPDATE .*SET .*WHERE")')" "true"
check "unconfirmed: wrote NOTHING" "$(rows_of "SELECT name FROM people WHERE id=1")" "Ada"

# ── confirmed ────────────────────────────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"row.write\",\"params\":{\"conn_id\":$CID,\"obj\":{\"name\":\"people\",\"schema\":\"main\"},\"key\":[[\"id\",1]],\"set\":[[\"name\",\"Ada L\"]],\"confirm\":true}}"
L=$(await '.id == 14')
check "confirmed: applied" "$(printf '%s' "$L" | jq '.result.applied')" "true"
check "confirmed: one row" "$(printf '%s' "$L" | jq '.result.affected')" "1"
check "confirmed: that row changed" "$(rows_of "SELECT name FROM people WHERE id=1")" "Ada L"
check "confirmed: the others did not" "$(rows_of "SELECT name FROM people WHERE id=2")" "Grace"

# ── a key that matches nothing ───────────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":15,\"method\":\"row.write\",\"params\":{\"conn_id\":$CID,\"obj\":{\"name\":\"people\",\"schema\":\"main\"},\"key\":[[\"id\",4242]],\"set\":[[\"name\",\"nobody\"]],\"confirm\":true}}"
L=$(await '.id == 15')
mentions "key matching nothing, refused" "$L" "nothing was written"
check "still three people" "$(rows_of "SELECT count(*) FROM people")" "3"

# ── a value that looks like SQL ──────────────────────────────────────────────
# If values were pasted into the statement rather than bound, this ends the
# UPDATE and drops the table. It must end up IN the column, as text.
INJECT="'); DROP TABLE people; --"
send "$(jq -nc --argjson c "$CID" --arg v "$INJECT" '{jsonrpc:"2.0",id:16,method:"row.write",params:{conn_id:$c,obj:{name:"people",schema:"main"},key:[["id",2]],set:[["note",$v]],confirm:true}}')"
L=$(await '.id == 16')
check "injection: applied as a value" "$(printf '%s' "$L" | jq '.result.applied')" "true"
check "injection: the table is still there" "$(rows_of "SELECT count(*) FROM people")" "3"
check "injection: stored verbatim" "$(rows_of "SELECT note FROM people WHERE id=2")" "$INJECT"

# ── delete ───────────────────────────────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":17,\"method\":\"row.write\",\"params\":{\"conn_id\":$CID,\"obj\":{\"name\":\"people\",\"schema\":\"main\"},\"key\":[[\"id\",3]],\"delete\":true,\"confirm\":true}}"
L=$(await '.id == 17')
check "delete: one row" "$(printf '%s' "$L" | jq '.result.affected')" "1"
check "delete: two left" "$(rows_of "SELECT count(*) FROM people")" "2"
check "delete: the right one went" "$(rows_of "SELECT count(*) FROM people WHERE id=3")" "0"

# ── a driver that does not claim edit ────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":18,\"method\":\"conn.connect\",\"params\":{\"driver\":\"duckdb\",\"params\":{\"file\":\"$WORK/d.duckdb\"},\"auth\":{\"kind\":\"none\"}}}"
DID=$(await '.id == 18' | jq '.result.conn_id // empty')
if [ -n "$DID" ]; then
    send "{\"jsonrpc\":\"2.0\",\"id\":19,\"method\":\"row.write\",\"params\":{\"conn_id\":$DID,\"obj\":{\"name\":\"x\"},\"key\":[[\"id\",1]],\"set\":[[\"a\",1]],\"confirm\":true}}"
    L=$(await '.id == 19')
    mentions "duckdb refuses to edit" "$L" "does not edit rows"
else
    printf '     %-42s %s\n' "duckdb not built in" "skipped"
fi

# ── the connection policy ────────────────────────────────────────────────────
# The policy lives on the CONNECTION and is enforced by the engine, so a client
# that skips its own confirmation dialog still cannot get past it. The three
# settings have to differ in the right places, not merely exist.
sqlite3 "$DB" "CREATE TABLE pol(id INTEGER PRIMARY KEY, a TEXT); INSERT INTO pol VALUES (1,'x');"

connect_with() {
    pid=$1; policy=$2
    send "{\"jsonrpc\":\"2.0\",\"id\":$pid,\"method\":\"conn.connect\",\"params\":{\"driver\":\"sqlite\",\"name\":\"pol-$policy\",\"params\":{\"file\":\"$DB\"},\"auth\":{\"kind\":\"none\"},\"policy\":\"$policy\"}}"
    await ".id == $pid" | jq '.result.conn_id'
}

RO=$(connect_with 40 read_only)
send "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"query.execute\",\"params\":{\"conn_id\":$RO,\"statement\":\"SELECT * FROM pol\"}}"
L=$(await '.id == 41')
check "read only: a SELECT runs" "$(printf '%s' "$L" | jq '.result.call_id != null')" "true"

# An INSERT is not DANGEROUS and is a write. Read-only has to refuse it, or
# "read only" would mean "no DROP", which is not what anybody means by it.
send "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"query.execute\",\"params\":{\"conn_id\":$RO,\"statement\":\"INSERT INTO pol VALUES (2,'y')\",\"confirm\":true}}"
L=$(await '.id == 42')
mentions "read only: refuses a harmless INSERT" "$L" "read only"
check "read only: nothing was inserted" "$(rows_of "SELECT count(*) FROM pol")" "1"

send "{\"jsonrpc\":\"2.0\",\"id\":43,\"method\":\"row.write\",\"params\":{\"conn_id\":$RO,\"obj\":{\"name\":\"pol\",\"schema\":\"main\"},\"key\":[[\"id\",1]],\"set\":[[\"a\",\"z\"]],\"confirm\":true}}"
L=$(await '.id == 43')
mentions "read only: refuses a row edit too" "$L" "read only"
check "read only: the row is unchanged" "$(rows_of "SELECT a FROM pol WHERE id=1")" "x"

CF=$(connect_with 44 confirm)
# Not dangerous: it runs without being asked about, because asking every time is
# how a confirmation becomes a reflex.
send "{\"jsonrpc\":\"2.0\",\"id\":45,\"method\":\"query.execute\",\"params\":{\"conn_id\":$CF,\"statement\":\"INSERT INTO pol VALUES (3,'c')\"}}"
L=$(await '.id == 45')
check "asks first: a plain INSERT runs" "$(printf '%s' "$L" | jq '.result.call_id != null')" "true"

send "{\"jsonrpc\":\"2.0\",\"id\":46,\"method\":\"query.execute\",\"params\":{\"conn_id\":$CF,\"statement\":\"DELETE FROM pol\"}}"
L=$(await '.id == 46')
mentions "asks first: unconfirmed DELETE refused" "$L" "needs confirming"
check "asks first: nothing was deleted" "$(rows_of "SELECT count(*) FROM pol")" "2"

send "{\"jsonrpc\":\"2.0\",\"id\":47,\"method\":\"query.execute\",\"params\":{\"conn_id\":$CF,\"statement\":\"DELETE FROM pol WHERE id=3\",\"confirm\":true}}"
L=$(await '.id == 47')
check "asks first: confirmed DELETE runs" "$(printf '%s' "$L" | jq '.result.call_id != null')" "true"

OP=$(connect_with 48 open)
send "{\"jsonrpc\":\"2.0\",\"id\":49,\"method\":\"query.execute\",\"params\":{\"conn_id\":$OP,\"statement\":\"DELETE FROM pol WHERE id=99\"}}"
L=$(await '.id == 49')
check "unrestricted: runs without asking" "$(printf '%s' "$L" | jq '.result.call_id != null')" "true"

# ── the audit log ────────────────────────────────────────────────────────────
# What was TRIED, not only what happened. The refusals have to be in here, or
# the log answers the easy question and not the one asked after something goes
# wrong. It is written by the engine on the path that does the work, so a client
# cannot leave itself out of it.
#
# Two statements that differ only in their VALUES first, so the fingerprint has
# something to group.
send "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"query.execute\",\"params\":{\"conn_id\":$OP,\"statement\":\"DELETE FROM pol WHERE id=771\"}}"
await '.id == 50' >/dev/null
send "{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"query.execute\",\"params\":{\"conn_id\":$OP,\"statement\":\"DELETE FROM pol WHERE id=772\"}}"
await '.id == 51' >/dev/null

# A statement with a CREDENTIAL in it. SQLite has no ALTER USER, so this fails —
# which is the point twice over: it proves a FAILED write is recorded, and it
# proves the password is not in the record.
send "{\"jsonrpc\":\"2.0\",\"id\":52,\"method\":\"query.execute\",\"params\":{\"conn_id\":$OP,\"statement\":\"ALTER USER app PASSWORD 'hunter2'\"}}"
await '.id == 52' >/dev/null
# query.execute answers with a call_id and finishes in the background, so wait
# for the state notification before asking the log about it.
await '.method == "query.state" and .params.state == "failed"' >/dev/null

# A connection CLOSED is part of the record of a connection opened.
send "{\"jsonrpc\":\"2.0\",\"id\":53,\"method\":\"conn.disconnect\",\"params\":{\"conn_id\":$OP}}"
await '.id == 53' >/dev/null

send '{"jsonrpc":"2.0","id":60,"method":"store.audit.list","params":{"limit":500}}'
A=$(await '.id == 60')
aq() { printf '%s' "$A" | jq "$@"; }

check "audit has rows" "$(aq '.result.audit | length > 0')" "true"
check "the statement mode is reported" "$(aq -r '.result.mode')" "full"
check "a refusal is recorded" \
    "$(aq '[.result.audit[] | select(.outcome=="refused")] | length > 0')" "true"
check "an applied write is recorded" \
    "$(aq '[.result.audit[] | select(.outcome=="applied")] | length > 0')" "true"
# The read-only refusal in particular: it is the one somebody will go looking for.
check "the read-only refusal is in it" \
    "$(aq '[.result.audit[] | select(.reason != null and (.reason | test("read only")))] | length > 0')" "true"
# The policy is recorded AS IT WAS. Changing a policy later must not rewrite what
# the rules were when something ran.
check "the policy at the time is kept" \
    "$(aq '[.result.audit[] | select(.policy=="read only")] | length > 0')" "true"
check "a row write names its path" \
    "$(aq '[.result.audit[] | select(.path=="row.write")] | length > 0')" "true"
# A SELECT is not a change and has no business in a log of changes.
check "reads are not in the audit" \
    "$(aq '[.result.audit[] | select(.statement | test("^SELECT"))] | length')" "0"

# ── which connection ─────────────────────────────────────────────────────────
# By NAME, not by conn_id: a handle means nothing once the process restarts, and
# the name is what somebody will search by.
check "the connection name is recorded" \
    "$(aq '[.result.audit[] | select(.conn=="rowtest")] | length > 0')" "true"
send '{"jsonrpc":"2.0","id":61,"method":"store.audit.list","params":{"limit":500,"conn":"pol-read_only"}}'
B=$(await '.id == 61')
check "narrowed to one connection" \
    "$(printf '%s' "$B" | jq '[.result.audit[] | select(.conn != "pol-read_only")] | length')" "0"
check "and that connection has rows" \
    "$(printf '%s' "$B" | jq '.result.audit | length > 0')" "true"

# ── the fingerprint ──────────────────────────────────────────────────────────
# The point of it: two runs differing only in a value are ONE thing in the log.
FP=$(aq -r '[.result.audit[] | select(.statement | test("DELETE FROM pol WHERE id=77"))] | map(.fingerprint) | unique | length')
check "same shape, different values, one fingerprint" "$FP" "1"
check "the two runs are both there" \
    "$(aq '[.result.audit[] | select(.statement | test("DELETE FROM pol WHERE id=77"))] | length')" "2"
# And the other half: a different statement is a different fingerprint.
check "a different statement is a different fingerprint" \
    "$(aq '[.result.audit[] | select(.path=="execute") | .fingerprint] | unique | length > 1')" "true"

# ── the secret ───────────────────────────────────────────────────────────────
# The whole log, as a single string: the password must not be in it ANYWHERE —
# not in the statement, not in the reason the engine gave back.
# Phrased so it cannot pass on an EMPTY log: the first clause is that there is
# a log at all. `null | tostring | test(...)` is false, and a green that means
# "there was nothing to find" is worse than no check.
check "the password is nowhere in the log" \
    "$(aq '(.result.audit | length > 0) and ((.result.audit | tostring | test("hunter2")) | not)')" "true"
check "but the statement that carried it IS" \
    "$(aq '[.result.audit[] | select(.statement | test("ALTER USER app PASSWORD"))] | length > 0')" "true"
check "and it is marked redacted" \
    "$(aq '[.result.audit[] | select(.statement | test("PASSWORD .<redacted>."))] | length > 0')" "true"
check "a write that FAILED is recorded as failed" \
    "$(aq '[.result.audit[] | select(.statement | test("ALTER USER")) | .outcome] | unique | .[0]')" '"failed"'

# ── connections ──────────────────────────────────────────────────────────────
check "opening a connection is recorded" \
    "$(aq '[.result.audit[] | select(.path=="conn.connect" and .outcome=="connected")] | length > 0')" "true"
check "closing one is recorded" \
    "$(aq '[.result.audit[] | select(.path=="conn.disconnect")] | length > 0')" "true"
check "the connect line says what it reached" \
    "$(aq '[.result.audit[] | select(.path=="conn.connect") | select(.statement | test("tls=prefer"))] | length > 0')" "true"
# A connect line is assembled from non-secret fields only, so there is nothing
# in it to redact — and nothing that looks like a credential either.
check "no credential field leaked into a connect line" \
    "$(aq '[.result.audit[] | select(.path=="conn.connect") | select(.statement | test("(?i)secret|password|token"))] | length')" "0"

# ── mongodb, when there is one to talk to ────────────────────────────────────
# The write path for mongo is WRITTEN but `caps.edit` is false, because nothing
# here can prove it: no mongod, no network, and the one real MongoDB in reach is
# somebody's data. Point $LODE_TEST_MONGO at a THROWAWAY server and this section
# runs; when it passes, `edit: true` goes in with the commit that shows it.
if [ -n "$LODE_TEST_MONGO" ]; then
    send "$(jq -nc --arg u "$LODE_TEST_MONGO" '{jsonrpc:"2.0",id:30,method:"conn.connect",params:{driver:"mongodb",params:{uri:$u,database:"lode_row_test"},auth:{kind:"none"}}}')"
    MID=$(await '.id == 30' | jq '.result.conn_id // empty')
    if [ -n "$MID" ]; then
        send "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"row.write\",\"params\":{\"conn_id\":$MID,\"obj\":{\"name\":\"t\"},\"key\":[[\"_id\",{\"__oid\":\"000000000000000000000000\"}]],\"set\":[[\"a\",1]],\"confirm\":true}}"
        L=$(await '.id == 31')
        # Until caps.edit is on, the engine refuses before reaching the driver —
        # which is itself the thing to check: the switch is what gates it.
        mentions "mongo is gated by caps.edit" "$L" "does not edit rows"
    else
        printf '     %-42s %s\n' "mongo: could not connect" "skipped"
    fi
else
    printf '     %-42s %s\n' "mongo: no \$LODE_TEST_MONGO" "skipped"
fi

exec 3>&-
wait $PID 2>/dev/null || true
if [ "$FAILED" = "0" ]; then
    echo "row: PASS"
else
    echo "row: FAIL"
    exit 1
fi
