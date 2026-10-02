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
"$BIN" < "$IN" > "$OUT" 2>"$WORK/err" &
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

send "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"conn.connect\",\"params\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"},\"auth\":{\"kind\":\"none\"}}}"
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
