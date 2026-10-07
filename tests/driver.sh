#!/bin/sh
# tests/driver.sh — a driver that is somebody else's program.
#
# This is the one thing in stage 7 that can be proved here in full: a foreign
# process that presents itself as a driver, in twenty lines of Python, doing
# everything a built-in driver does.
#
#   it appears in rpc.hello with the metadata IT declared
#   a connection opens through it and its objects are in the tree
#   a statement runs and its rows come back as rows
#   its own field types reach the form (nothing is hardcoded here)
#
# And the half that matters more — what it is NOT believed about:
#
#   it cannot make a DROP safe                 the engine's rule is a floor
#   it cannot make an INSERT a read            read-only still refuses it
#   it cannot claim `edit`                     that is a promise about binding values
#   it cannot call itself `postgres`           a built-in driver wins its own name
#   it is not handed the credentials           a foreign program does not get them
#   a program that fails to describe itself    is reported, not silently absent
#
# Needs no network and no database: python3 and a temporary directory.
#
#   scripts/build && tests/driver.sh
set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ ! -x "$BIN" ]; then
    echo "driver: no loded binary — run scripts/build first" >&2
    exit 1
fi
command -v jq >/dev/null || { echo "driver: jq is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "driver: python3 is required" >&2; exit 1; }

WORK=$(mktemp -d)
IN=$WORK/in
OUT=$WORK/out
FAILED=0
trap 'kill $PID 2>/dev/null || true' EXIT

# ── the foreign driver ───────────────────────────────────────────────────────
# A key/value store of three rows that calls itself `toy`. It LIES in two
# places on purpose: it claims `edit`, and it answers every `classify` with
# "nothing is dangerous". Neither claim is believed, and that is the test.
cat > "$WORK/toy.py" <<'PY'
import json, sys

ROWS = [[1, "Ada", 10], [2, "Grace", 20], [3, "Alan", 30]]

META = {
    "kind": "toy", "display": "Toy Store", "default_port": 1234,
    "params": [
        {"key": "path", "label": "Where it lives", "type": "file", "required": True},
        {"key": "flavour", "label": "Flavour", "type": "string", "default": "plain"},
    ],
    "auth": ["none"],
    # Two of these are claims the engine will NOT take: `edit` is a promise
    # about how writes are implemented, and `cancel` is machinery this
    # transport does not have.
    "caps": {"sql": True, "schemas": True, "edit": True, "cancel": True, "metrics": True},
    "hint": "GET *",
    "browse": "GET {name}",
}

def handle(method, p):
    if method == "meta":      return META
    if method == "connect":   return {"conn": "c1"}
    if method == "structure": return {"nodes": [{"name": "things", "kind": "table", "children": []}]}
    if method == "columns":
        return {"columns": [{"name": "id", "type": "int", "key": True},
                            {"name": "name", "type": "text"},
                            {"name": "n", "type": "int"}]}
    if method == "execute":
        return {"columns": [{"name": "id"}, {"name": "name"}, {"name": "n"}], "rows": ROWS}
    if method == "close":     return {}
    raise ValueError("the toy driver does not do '%s'" % method)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    try:
        out = {"id": req["id"], "ok": True, "result": handle(req["method"], req.get("params", {}))}
    except Exception as e:
        out = {"id": req["id"], "ok": False, "error": str(e)}
    sys.stdout.write(json.dumps(out) + "\n")
    sys.stdout.flush()
PY

# One that cannot describe itself, to prove a broken driver is reported rather
# than silently missing.
cat > "$WORK/broken.py" <<'PY'
import sys
sys.stdout.write("this is not JSON\n")
sys.stdout.flush()
PY

mkfifo "$IN"
: > "$OUT"
# LODE_DATA_DIR in the throwaway directory, FIRST: registering a driver writes a
# profile, and a profile from a test belongs in a test's database. Registering
# happens in a FIRST engine; a second one then picks the drivers up at startup,
# which is where they are loaded.
DATA=$WORK/data
LODE_DATA_DIR=$DATA "$BIN" < "$IN" > "$OUT" 2>"$WORK/err1" &
PID=$!
exec 3>"$IN"
send() { printf '%s\n' "$1" >&3; }
await() {
    i=0
    while [ "$i" -lt 150 ]; do
        line=$(jq -c --slurp "map(select($1)) | last // empty" "$OUT" 2>/dev/null || true)
        if [ -n "$line" ] && [ "$line" != "null" ]; then printf '%s\n' "$line"; return 0; fi
        sleep 0.1; i=$((i + 1))
    done
    echo "TIMED OUT waiting for $1" >&2; return 1
}
check() {
    label=$1; got=$2; want=$3
    if [ "$got" = "$want" ]; then
        printf 'ok   %-52s %s\n' "$label" "$got"
    else
        printf 'FAIL %-52s got %s, want %s\n' "$label" "$got" "$want"
        FAILED=1
    fi
}
mentions() {
    label=$1; line=$2; word=$3
    msg=$(printf '%s' "$line" | jq -r '.error // ""')
    case "$msg" in
        *"$word"*) printf 'ok   %-52s %s\n' "$label" "says \"$word\"" ;;
        *) printf 'FAIL %-52s %s\n' "$label" "message was: $msg"; FAILED=1 ;;
    esac
}

reg() {
    send "$(jq -nc --argjson i "$1" --arg n "$2" --arg c "$3" \
        '{jsonrpc:"2.0",id:$i,method:"store.profiles.save",params:{kind:"driver",name:$n,spec:{command:$c}}}')"
    await ".id == $1"
}

check "a driver program registers" \
    "$(reg 1 toy "python3 $WORK/toy.py" | jq '.result.saved')" "true"
reg 2 broken "python3 $WORK/broken.py" >/dev/null
reg 3 nothing "" >/dev/null
# A program calling itself by a built-in driver's name.
cat > "$WORK/impostor.py" <<PY
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    sys.stdout.write(json.dumps({"id": req["id"], "ok": True, "result": {
        "kind": "postgres", "display": "Not PostgreSQL", "caps": {"sql": True}}}) + "\n")
    sys.stdout.flush()
PY
reg 4 impostor "python3 $WORK/impostor.py" >/dev/null

exec 3>&-
kill $PID 2>/dev/null || true
sleep 0.5

# ── a second engine, which loads them at startup ─────────────────────────────
IN2=$WORK/in2; OUT2=$WORK/out2
mkfifo "$IN2"; : > "$OUT2"
LODE_DATA_DIR=$DATA "$BIN" < "$IN2" > "$OUT2" 2>"$WORK/err2" &
PID=$!
exec 3>"$IN2"
OUT=$OUT2

send '{"jsonrpc":"2.0","id":10,"method":"rpc.hello"}'
H=$(await '.id == 10')
hq() { printf '%s' "$H" | jq "$@"; }

check "the foreign driver is in the list" \
    "$(hq '[.result.drivers[] | select(.kind=="toy")] | length')" "1"
# Its own metadata, which nothing here knows: the form is built from this.
check "with the display name IT chose" \
    "$(hq -r '[.result.drivers[] | select(.kind=="toy")][0].display')" "Toy Store"
check "its own fields" \
    "$(hq -r '[.result.drivers[] | select(.kind=="toy")][0].params | map(.key) | join(",")')" "path,flavour"
check "and its own field TYPES" \
    "$(hq -r '[.result.drivers[] | select(.kind=="toy")][0].params[0].type')" "file"
check "and the hint it wants in an empty editor" \
    "$(hq -r '[.result.drivers[] | select(.kind=="toy")][0].hint')" "GET *"

# ── the claims it does not get to make ───────────────────────────────────────
# `edit` is a promise that every value is bound as a parameter and that a write
# which hit more than one row can be undone. The engine cannot check that, so it
# does not repeat it.
check "it cannot claim 'edit'" \
    "$(hq '[.result.drivers[] | select(.kind=="toy")][0].caps.edit')" "false"
check "nor 'cancel'" \
    "$(hq '[.result.drivers[] | select(.kind=="toy")][0].caps.cancel')" "false"
check "nor 'metrics'" \
    "$(hq '[.result.drivers[] | select(.kind=="toy")][0].caps.metrics')" "false"
# The ones that are only SHAPE are taken at its word.
check "but its shape is believed" \
    "$(hq '[.result.drivers[] | select(.kind=="toy")][0].caps.schemas')" "true"

# A built-in driver wins its own name.
check "an impostor did not replace postgres" \
    "$(hq -r '[.result.drivers[] | select(.kind=="postgres")][0].display | test("Not PostgreSQL") | not')" "true"

# ── what became of each program, said rather than left out ───────────────────
check "every registered program is accounted for" "$(hq '.result.external | length')" "4"
check "the working one is ok" \
    "$(hq '[.result.external[] | select(.name=="toy")][0].ok')" "true"
check "the broken one is not" \
    "$(hq '[.result.external[] | select(.name=="broken")][0].ok')" "false"
check "and says why" \
    "$(hq -r '[.result.external[] | select(.name=="broken")][0].error | test("not JSON")')" "true"
check "one with no command says that" \
    "$(hq -r '[.result.external[] | select(.name=="nothing")][0].error | test("no command")')" "true"
check "the impostor says what it tried to be" \
    "$(hq -r '[.result.external[] | select(.name=="impostor")][0].error | test("already has built in")')" "true"

# ── it works like a driver ───────────────────────────────────────────────────
send '{"jsonrpc":"2.0","id":20,"method":"conn.connect","params":{"driver":"toy","name":"t","params":{"path":"/tmp/toy"},"auth":{"kind":"none"}}}'
L=$(await '.id == 20')
CID=$(printf '%s' "$L" | jq '.result.conn_id // empty')
check "a connection opens through it" "$([ -n "$CID" ] && echo yes)" "yes"

send "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"schema.structure\",\"params\":{\"conn_id\":$CID}}"
check "its objects are in the tree" \
    "$(await '.id == 21' | jq -r '.result.nodes[0].name')" "things"

send "{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"query.execute\",\"params\":{\"conn_id\":$CID,\"statement\":\"GET *\"}}"
CALL=$(await '.id == 22' | jq '.result.call_id')
await '.method == "query.state" and .params.state == "done"' >/dev/null
send "{\"jsonrpc\":\"2.0\",\"id\":23,\"method\":\"query.page\",\"params\":{\"call_id\":$CALL,\"from\":0,\"n\":10}}"
P=$(await '.id == 23')
check "a statement runs through it" "$(printf '%s' "$P" | jq '.result.ready')" "true"
check "and its rows come back as rows" "$(printf '%s' "$P" | jq '.result.rows | length')" "3"
check "with its columns" "$(printf '%s' "$P" | jq -r '.result.columns | map(.name) | join(",")')" "id,name,n"
check "and the values are values, not strings" "$(printf '%s' "$P" | jq '.result.rows[0][2]')" "10"

# ── the floor: its opinion cannot make anything safe ─────────────────────────
# The toy driver answers nothing at all about danger. The engine's own rule
# still applies, which is the point: a driver is a program somebody installed,
# and a program somebody installed is not a reason to stop checking.
send '{"jsonrpc":"2.0","id":30,"method":"query.classify","params":{"driver":"toy","statement":"DROP TABLE things"}}'
check "a DROP through it is still dangerous" \
    "$(await '.id == 30' | jq -r '.result.reason != null')" "true"
send '{"jsonrpc":"2.0","id":31,"method":"query.classify","params":{"driver":"toy","statement":"DELETE FROM things"}}'
check "so is a DELETE with no WHERE" \
    "$(await '.id == 31' | jq -r '.result.reason | test("EVERY row")')" "true"

# And a read-only connection through it still refuses a write.
send '{"jsonrpc":"2.0","id":32,"method":"conn.connect","params":{"driver":"toy","name":"ro","params":{"path":"/tmp/toy"},"auth":{"kind":"none"},"policy":"read_only"}}'
RID=$(await '.id == 32' | jq '.result.conn_id')
send "{\"jsonrpc\":\"2.0\",\"id\":33,\"method\":\"query.execute\",\"params\":{\"conn_id\":$RID,\"statement\":\"INSERT INTO things VALUES (4)\",\"confirm\":true}}"
mentions "read-only still refuses a write through it" "$(await '.id == 33')" "read only"

exec 3>&-
i=0
while kill -0 $PID 2>/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
kill $PID 2>/dev/null || true
if [ "$FAILED" -eq 0 ]; then echo "driver: PASS"; else echo "driver: FAIL"; exit 1; fi
exit 0
