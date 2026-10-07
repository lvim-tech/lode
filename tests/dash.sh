#!/bin/sh
# tests/dash.sh — prove that the engine will not store a broken dashboard.
#
# A dashboard is an arrangement of SAVED CHARTS on a grid of twelve columns.
# What is worth testing is not that a correct board saves — it is that the four
# ways a board can be wrong are all refused HERE, in the engine, with the name
# of the thing that is wrong in the message:
#
#   a tile naming a chart that does not exist     refused, and names the chart
#   a tile running past the twelfth column        refused, and says where the edge is
#   a tile with no size                           refused
#   two tiles on the same cell                    refused, and names both
#   a time range that is not two timestamps       refused
#   a time range that runs backwards              refused
#
# A client could check every one of them. The point is that it does not have to
# be trusted to: a board saved wrong is wrong every time it is opened, by every
# client, for as long as it exists.
#
# Also: a chart REFERENCED BY NAME survives its chart being renamed around it —
# the board says which chart is missing rather than drawing a different one.
#
# Needs no network and no database server: a throwaway SQLite file and `jq`.
#
#   scripts/build && tests/dash.sh
set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ ! -x "$BIN" ]; then
    echo "dash: no loded binary — run scripts/build first" >&2
    exit 1
fi
command -v jq >/dev/null || { echo "dash: jq is required" >&2; exit 1; }

WORK=$(mktemp -d)
IN=$WORK/in
OUT=$WORK/out
FAILED=0
trap 'kill $PID 2>/dev/null || true' EXIT

mkfifo "$IN"
: > "$OUT"
# Never the real store.
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
        printf 'ok   %-46s %s\n' "$label" "$got"
    else
        printf 'FAIL %-46s got %s, want %s\n' "$label" "$got" "$want"
        FAILED=1
    fi
}

# A refusal whose MESSAGE has to contain something: a board that is rejected
# without saying WHICH tile is wrong is a board nobody can repair.
mentions() {
    label=$1; line=$2; word=$3
    msg=$(printf '%s' "$line" | jq -r '.error // ""')
    case "$msg" in
        *"$word"*) printf 'ok   %-46s %s\n' "$label" "says \"$word\"" ;;
        *) printf 'FAIL %-46s %s\n' "$label" "message was: $msg"; FAILED=1 ;;
    esac
}

# A board, as one JSON argument.
save() {
    id=$1; name=$2; board=$3
    send "$(jq -nc --arg n "$name" --argjson b "$board" --argjson i "$id" \
        '{jsonrpc:"2.0",id:$i,method:"store.dashboards.save",params:{conn:"demo",name:$n,dashboard:$b}}')"
    await ".id == $id"
}

send '{"jsonrpc":"2.0","id":1,"method":"rpc.hello"}'
HELLO=$(await '.id == 1')
check "protocol carries dashboards" "$(printf '%s' "$HELLO" | jq '.result.proto >= 8')" "true"
# Each engine says how IT spells a time window. The client never invents one.
check "postgres declares a time filter" \
    "$(printf '%s' "$HELLO" | jq -r '[.result.drivers[] | select(.kind=="postgres")][0].time_filter')" \
    "{col} >= '{from}' AND {col} < '{to}'"
check "mongodb declares its own, as a document" \
    "$(printf '%s' "$HELLO" | jq '[.result.drivers[] | select(.kind=="mongodb")][0].time_filter | index("$gte") != null')" "true"
# Redis has no time window. An empty template is how a driver says so — it is
# not a gap, it is the answer.
check "redis declares none, because it has none" \
    "$(printf '%s' "$HELLO" | jq -r '[.result.drivers[] | select(.kind=="redis")][0].time_filter')" ""

# ── two saved charts to build a board out of ─────────────────────────────────
send '{"jsonrpc":"2.0","id":2,"method":"store.charts.save","params":{"conn":"demo","name":"revenue","sql":"SELECT 1","measure":"n"}}'
await '.id == 2' >/dev/null
send '{"jsonrpc":"2.0","id":3,"method":"store.charts.save","params":{"conn":"demo","name":"signups","sql":"SELECT 2","measure":"n"}}'
await '.id == 3' >/dev/null

# ── a board that is right ────────────────────────────────────────────────────
GOOD='{"tiles":[
  {"kind":"chart","chart":"revenue","x":0,"y":0,"w":6,"h":4},
  {"kind":"chart","chart":"signups","x":6,"y":0,"w":6,"h":4},
  {"kind":"divider","x":0,"y":4,"w":12,"h":1},
  {"kind":"text","text":"Quarter to date","x":0,"y":5,"w":12,"h":2}
],"time":{"column":"created_at","from":"2026-01-05","to":"2026-02-01 14:30Z"}}'
L=$(save 10 "ops" "$GOOD")
check "a correct board saves" "$(printf '%s' "$L" | jq '.result.saved')" "true"
# Stored canonically, so a range cannot read one way on save and another on load.
check "the time range comes back canonical" \
    "$(printf '%s' "$L" | jq -r '.result.dashboard.time.to')" "2026-02-01T14:30:00"

send '{"jsonrpc":"2.0","id":11,"method":"store.dashboards.list","params":{"conn":"demo"}}'
L=$(await '.id == 11')
check "it is listed" "$(printf '%s' "$L" | jq -r '.result.dashboards[0].name')" "ops"
# A list is for choosing from. Shipping every board's whole layout to draw a
# list of names is a page of JSON per row nobody reads.
check "the list carries no layout" "$(printf '%s' "$L" | jq '.result.dashboards[0] | has("tiles")')" "false"

send '{"jsonrpc":"2.0","id":12,"method":"store.dashboards.get","params":{"conn":"demo","name":"ops"}}'
L=$(await '.id == 12')
check "it reads back whole" "$(printf '%s' "$L" | jq '.result.dashboard.tiles | length')" "4"
check "the grid is twelve columns" "$(printf '%s' "$L" | jq '.result.columns')" "12"
check "a correct board is not stale" "$(printf '%s' "$L" | jq '.result.stale')" "null"
# BY NAME, not by index.
check "a tile names its chart" "$(printf '%s' "$L" | jq -r '.result.dashboard.tiles[0].chart')" "revenue"
check "a missing board is null, not an error" \
    "$(send '{"jsonrpc":"2.0","id":13,"method":"store.dashboards.get","params":{"conn":"demo","name":"nope"}}'; await '.id == 13' | jq '.result.dashboard')" "null"

# ── the four ways a board is wrong ───────────────────────────────────────────
L=$(save 20 "bad1" '{"tiles":[{"kind":"chart","chart":"profit","x":0,"y":0,"w":6,"h":4}]}')
mentions "a chart that does not exist, refused" "$L" "profit"
mentions "and says what is wrong with it" "$L" "does not exist"

L=$(save 21 "bad2" '{"tiles":[{"kind":"chart","chart":"revenue","x":8,"y":0,"w":6,"h":4}]}')
mentions "a tile past the right edge, refused" "$L" "12 columns"

L=$(save 22 "bad3" '{"tiles":[{"kind":"divider","x":0,"y":0,"w":12,"h":0}]}')
mentions "a tile with no size, refused" "$L" "no size"

L=$(save 23 "bad4" '{"tiles":[
  {"kind":"chart","chart":"revenue","x":0,"y":0,"w":6,"h":4},
  {"kind":"chart","chart":"signups","x":4,"y":2,"w":6,"h":4}]}')
mentions "two tiles on one cell, refused" "$L" "overlap"
mentions "and both are named" "$L" "signups"

# None of the four left anything behind.
send '{"jsonrpc":"2.0","id":24,"method":"store.dashboards.list","params":{"conn":"demo"}}'
check "nothing broken was stored" \
    "$(await '.id == 24' | jq '.result.dashboards | length')" "1"

# ── the time range ───────────────────────────────────────────────────────────
# This is a SAFETY check, not a parsing one: what reaches the driver's template
# is pasted into a statement, so anything that is not exactly a timestamp has to
# be refused before it gets there.
L=$(save 30 "t1" '{"tiles":[],"time":{"column":"ts","from":"yesterday","to":"2026-01-01"}}')
mentions "a range end that is not a timestamp, refused" "$L" "not a timestamp"

L=$(save 31 "t2" '{"tiles":[],"time":{"column":"ts","from":"2026-01-05'"'"'; DROP TABLE people; --","to":"2026-02-01"}}')
mentions "SQL in a range end, refused" "$L" "not a timestamp"

L=$(save 32 "t3" '{"tiles":[],"time":{"column":"ts","from":"2026-02-01","to":"2026-01-01"}}')
mentions "a range that runs backwards, refused" "$L" "ends before it starts"

L=$(save 33 "t4" '{"tiles":[],"time":{"column":"","from":"2026-01-01","to":"2026-02-01"}}')
mentions "a range with no column, refused" "$L" "column"

# ── a chart that goes away under a board ─────────────────────────────────────
# The reason tiles name charts rather than index them. The board must say WHICH
# chart is gone — and must still open, because a layout somebody spent time on
# should be repairable, not withheld.
send '{"jsonrpc":"2.0","id":40,"method":"store.charts.remove","params":{"conn":"demo","name":"signups"}}'
await '.id == 40' >/dev/null
send '{"jsonrpc":"2.0","id":41,"method":"store.dashboards.get","params":{"conn":"demo","name":"ops"}}'
L=$(await '.id == 41')
check "the board still opens" "$(printf '%s' "$L" | jq '.result.dashboard.tiles | length')" "4"
check "and it says it is stale" "$(printf '%s' "$L" | jq '.result.stale != null')" "true"
check "naming the chart that went away" \
    "$(printf '%s' "$L" | jq -r '.result.stale | test("signups")')" "true"

# ── boards are per connection, and removable ─────────────────────────────────
send '{"jsonrpc":"2.0","id":50,"method":"store.dashboards.list","params":{"conn":"other"}}'
check "boards are scoped to their connection" \
    "$(await '.id == 50' | jq '.result.dashboards | length')" "0"
send '{"jsonrpc":"2.0","id":51,"method":"store.dashboards.remove","params":{"conn":"demo","name":"ops"}}'
check "removed" "$(await '.id == 51' | jq '.result.removed')" "true"
send '{"jsonrpc":"2.0","id":52,"method":"store.dashboards.remove","params":{"conn":"demo","name":"ops"}}'
check "removing it twice is false, not an error" \
    "$(await '.id == 52' | jq '.result.removed')" "false"

exec 3>&-
wait $PID 2>/dev/null || true
if [ "$FAILED" -eq 0 ]; then echo "dash: PASS"; else echo "dash: FAIL"; exit 1; fi
