#!/bin/sh
# tests/hook.sh — somebody's own code, run around a connection, and scripts.
#
# A hook is a script the engine runs at a moment it chose; a script is a hook
# somebody ran themselves. What is worth proving about them:
#
#   a PreConnect hook that FAILS stops the connection        and says so
#   the other three do not stop anything                     there is nothing left to stop
#   every hook is in the audit log, worked or not            it is somebody's code on the engine's initiative
#   the environment carries CONTEXT and NO credential        `ps` can read a child's environment
#   output arrives WHILE it runs, with its stream named      which stream is most of what a failure says
#   a script that overstays is stopped, and says it was      not reported as a plain failure
#   a hook naming a script that is gone says WHICH           a hook that quietly does nothing protects nothing
#
# Needs no network and no database server: a throwaway SQLite file and `jq`.
#
#   scripts/build && tests/hook.sh
set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ ! -x "$BIN" ]; then
    echo "hook: no loded binary — run scripts/build first" >&2
    exit 1
fi
command -v jq >/dev/null || { echo "hook: jq is required" >&2; exit 1; }
command -v sqlite3 >/dev/null || { echo "hook: sqlite3 is required" >&2; exit 1; }

WORK=$(mktemp -d)
DB=$WORK/h.sqlite
IN=$WORK/in
OUT=$WORK/out
FAILED=0
trap 'kill $PID 2>/dev/null || true' EXIT

sqlite3 "$DB" "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1);"

mkfifo "$IN"
: > "$OUT"
# LODE_DATA_DIR in the throwaway directory, FIRST: this test makes the engine
# save scripts and write audit rows, and both belong in a test's database.
LODE_DATA_DIR=$WORK/data "$BIN" < "$IN" > "$OUT" 2>"$WORK/err" &
PID=$!
exec 3>"$IN"

send() { printf '%s\n' "$1" >&3; }

await() {
    filter=$1
    i=0
    while [ "$i" -lt 200 ]; do
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

# Every line matching a filter, not only the last — for streamed output.
all() {
    jq -c --slurp "map(select($1))" "$OUT" 2>/dev/null || echo "[]"
}

check() {
    label=$1; got=$2; want=$3
    if [ "$got" = "$want" ]; then
        printf 'ok   %-50s %s\n' "$label" "$got"
    else
        printf 'FAIL %-50s got %s, want %s\n' "$label" "$got" "$want"
        FAILED=1
    fi
}

mentions() {
    label=$1; line=$2; word=$3
    msg=$(printf '%s' "$line" | jq -r '.error // ""')
    case "$msg" in
        *"$word"*) printf 'ok   %-50s %s\n' "$label" "says \"$word\"" ;;
        *) printf 'FAIL %-50s %s\n' "$label" "message was: $msg"; FAILED=1 ;;
    esac
}

conn_with_hooks() {
    id=$1; hooks=$2
    send "$(jq -nc --arg f "$DB" --argjson h "$hooks" --argjson i "$id" \
        '{jsonrpc:"2.0",id:$i,method:"conn.connect",params:{driver:"sqlite",name:"hooked",params:{file:$f},auth:{kind:"none"},hooks:$h}}')"
    await ".id == $id"
}

send '{"jsonrpc":"2.0","id":1,"method":"rpc.hello"}'
check "protocol carries hooks and scripts" \
    "$(await '.id == 1' | jq '.result.proto >= 10')" "true"

# ── a PreConnect hook that fails STOPS the connection ────────────────────────
# The only reason to have a hook before rather than after.
L=$(conn_with_hooks 10 '[{"on":"pre_connect","kind":"command","source":"echo refusing >&2; exit 7"}]')
mentions "a failing pre_connect stops the connect" "$L" "pre_connect hook failed"
mentions "and says the connection was not opened" "$L" "NOT opened"
mentions "and keeps the exit status" "$L" "status 7"
check "no connection was handed back" "$(printf '%s' "$L" | jq '.result.conn_id // null')" "null"

# Its output was streamed, not held until the end: a hook that is going to stop
# a connection should be readable WHILE it runs.
check "its output arrived as a notification" \
    "$(all '.method == "script.output" and .params.text == "refusing"' | jq 'length > 0')" "true"
check "and names the stream it came from" \
    "$(all '.method == "script.output" and .params.text == "refusing"' | jq -r '.[0].params.stream')" "stderr"
check "and which hook it was" \
    "$(all '.method == "script.output" and .params.text == "refusing"' | jq -r '.[0].params.hook')" "pre_connect"

# ── a PreConnect hook that succeeds lets it through ──────────────────────────
L=$(conn_with_hooks 11 '[{"on":"pre_connect","kind":"command","source":"true"}]')
check "a passing pre_connect lets the connect happen" \
    "$(printf '%s' "$L" | jq '.result.conn_id != null')" "true"
CID=$(printf '%s' "$L" | jq '.result.conn_id')

# ── the environment: context, and no credential ──────────────────────────────
send "$(jq -nc --arg f "$DB" \
    '{jsonrpc:"2.0",id:12,method:"conn.connect",params:{driver:"sqlite",name:"envtest",params:{file:$f,host:"db.internal",port:"6543",database:"app"},auth:{kind:"password",user:"alice",password:"hunter2"},hooks:[{"on":"pre_connect","kind":"command","source":"echo \"ENV[$LODE_EVENT|$LODE_CONN|$LODE_DRIVER|$LODE_HOST|$LODE_PORT|$LODE_DATABASE]\"; env | sort | tr \"\\n\" \" \""}]}}')"
await '.id == 12' >/dev/null
ENVLINE=$(all '.method == "script.output" and (.params.text | startswith("ENV["))' | jq -r '.[0].params.text')
check "the hook is told the event" "$(printf '%s' "$ENVLINE" | grep -c 'pre_connect')" "1"
check "and the connection, driver, host, port and database" \
    "$ENVLINE" 'ENV[pre_connect|envtest|sqlite|db.internal|6543|app]'
# THE one that matters. A child's environment is readable from `ps` and from
# /proc, and by everything the child goes on to start.
DUMP=$(all '.method == "script.output"' | jq -r '[.[].params.text] | join(" ")')
check "and NO credential is anywhere in its environment" \
    "$(printf '%s' "$DUMP" | grep -c 'hunter2' || true)" "0"
check "nor the user name under a LODE_ variable" \
    "$(printf '%s' "$DUMP" | grep -co 'LODE_PASSWORD\|LODE_USER' || true)" "0"

# ── the other three do not stop anything ─────────────────────────────────────
L=$(conn_with_hooks 20 '[{"on":"post_connect","kind":"command","source":"exit 4"}]')
check "a failing post_connect does NOT undo the connection" \
    "$(printf '%s' "$L" | jq '.result.conn_id != null')" "true"
DID=$(printf '%s' "$L" | jq '.result.conn_id')
send "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"conn.disconnect\",\"params\":{\"conn_id\":$DID}}"
check "and the disconnect still works" "$(await '.id == 21' | jq '.result != null')" "true"

# A failing pre_disconnect must not keep a connection open that somebody asked
# to close — the hook would be deciding something that is not its decision.
L=$(conn_with_hooks 22 '[{"on":"pre_disconnect","kind":"command","source":"exit 5"},{"on":"post_disconnect","kind":"command","source":"echo bye"}]')
XID=$(printf '%s' "$L" | jq '.result.conn_id')
send "{\"jsonrpc\":\"2.0\",\"id\":23,\"method\":\"conn.disconnect\",\"params\":{\"conn_id\":$XID}}"
check "a failing pre_disconnect still disconnects" "$(await '.id == 23' | jq '.result != null')" "true"
check "and post_disconnect ran after it" \
    "$(all '.method == "script.output" and .params.text == "bye"' | jq 'length > 0')" "true"

# ── scripts as documents ─────────────────────────────────────────────────────
send '{"jsonrpc":"2.0","id":30,"method":"store.scripts.save","params":{"conn":"hooked","name":"greet","lang":"bash","body":"echo one\necho two\nexit 0"}}'
check "a script saves" "$(await '.id == 30' | jq '.result.saved')" "true"
send '{"jsonrpc":"2.0","id":31,"method":"store.scripts.list","params":{"conn":"hooked"}}'
L=$(await '.id == 31')
check "it is listed with its language" "$(printf '%s' "$L" | jq -r '.result.scripts[0].lang')" "bash"
check "and its body" "$(printf '%s' "$L" | jq -r '.result.scripts[0].body | split("\n")[0]')" "echo one"

send '{"jsonrpc":"2.0","id":32,"method":"script.run","params":{"conn":"hooked","name":"greet"}}'
L=$(await '.id == 32')
RUN=$(printf '%s' "$L" | jq '.result.run_id')
check "running one answers with an id straight away" "$([ -n "$RUN" ] && echo yes)" "yes"
await ".method == \"script.state\" and .params.run_id == $RUN" >/dev/null
check "it finished" "$(await ".method == \"script.state\" and .params.run_id == $RUN" | jq -r '.params.state')" "done"
check "both lines of output arrived" \
    "$(all ".method == \"script.output\" and .params.run_id == $RUN" | jq 'length')" "2"

# A global script (conn '') is reachable from a connection that has none of its
# own — hiding them would make them findable only by knowing they exist.
send '{"jsonrpc":"2.0","id":33,"method":"store.scripts.save","params":{"name":"global","lang":"bash","body":"echo everywhere"}}'
await '.id == 33' >/dev/null
send '{"jsonrpc":"2.0","id":34,"method":"store.scripts.list","params":{"conn":"someone-else"}}'
check "a global script is visible from any connection" \
    "$(await '.id == 34' | jq '[.result.scripts[] | select(.name=="global")] | length')" "1"

# Both a name and a body is refused rather than picked between: either choice
# would silently run something other than what was asked for.
send '{"jsonrpc":"2.0","id":35,"method":"script.run","params":{"name":"greet","body":"echo x","lang":"bash"}}'
mentions "a name AND a body is refused" "$(await '.id == 35')" "not both and not neither"
send '{"jsonrpc":"2.0","id":36,"method":"script.run","params":{"body":"print(1)"}}'
mentions "a body with no language is refused" "$(await '.id == 36')" "which language"
send '{"jsonrpc":"2.0","id":37,"method":"script.run","params":{"name":"nope"}}'
mentions "a script that does not exist says its name" "$(await '.id == 37')" "'nope'"

# A failing script is failed, with the status.
send '{"jsonrpc":"2.0","id":38,"method":"script.run","params":{"body":"echo last words; exit 9","lang":"bash"}}'
R2=$(await '.id == 38' | jq '.result.run_id')
S2=$(await ".method == \"script.state\" and .params.run_id == $R2")
check "a failing script is failed" "$(printf '%s' "$S2" | jq -r '.params.state')" "failed"
check "with its exit status" "$(printf '%s' "$S2" | jq '.params.code')" "9"
# Output written just before exiting is usually the reason for the exit.
check "and the last thing it wrote is not lost" \
    "$(all ".method == \"script.output\" and .params.run_id == $R2" | jq -r '.[0].params.text')" "last words"

# One that overstays is STOPPED, and says that is what happened rather than
# being reported as a plain failure.
send '{"jsonrpc":"2.0","id":39,"method":"script.run","params":{"body":"sleep 30","lang":"bash","timeout_ms":400}}'
R3=$(await '.id == 39' | jq '.result.run_id')
S3=$(await ".method == \"script.state\" and .params.run_id == $R3")
check "an overstaying script is stopped" "$(printf '%s' "$S3" | jq -r '.params.state')" "failed"
check "and says it ran out of time" \
    "$(printf '%s' "$S3" | jq -r '.params.error | test("ran out of time")')" "true"

# ── a hook naming a script that is gone ──────────────────────────────────────
# By NAME, so the message can say which. A hook that quietly did nothing because
# its script was deleted is a hook that stopped protecting anything.
L=$(conn_with_hooks 40 '[{"on":"pre_connect","kind":"script","source":"vanished"}]')
mentions "a hook naming a missing script says which" "$L" "'vanished'"
mentions "and still stops the connection" "$L" "NOT opened"

# ── all of it is in the audit log ────────────────────────────────────────────
send '{"jsonrpc":"2.0","id":50,"method":"store.audit.list","params":{"limit":300}}'
A=$(await '.id == 50')
aq() { printf '%s' "$A" | jq "$@"; }
check "hooks are audited" "$(aq '[.result.audit[] | select(.path | startswith("hook."))] | length > 0')" "true"
check "a hook that FAILED is in it" \
    "$(aq '[.result.audit[] | select(.path == "hook.pre_connect" and .outcome == "failed")] | length > 0')" "true"
check "a hook that worked is in it too" \
    "$(aq '[.result.audit[] | select(.path | startswith("hook.")) | select(.outcome == "applied")] | length > 0')" "true"
check "each of the four events is its own path" \
    "$(aq '[.result.audit[] | select(.path | startswith("hook.")) | .path] | unique | length >= 3')" "true"
check "scripts are audited" "$(aq '[.result.audit[] | select(.path == "script")] | length > 0')" "true"
check "a failing script is audited as failed" \
    "$(aq '[.result.audit[] | select(.path == "script" and .outcome == "failed")] | length > 0')" "true"
# And the password from the env test is nowhere, as everywhere else.
check "no credential reached the log" \
    "$(aq '(.result.audit | length > 0) and ((.result.audit | tostring | test("hunter2")) | not)')" "true"

exec 3>&-
i=0
while kill -0 $PID 2>/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
kill $PID 2>/dev/null || true
if [ "$FAILED" -eq 0 ]; then echo "hook: PASS"; else echo "hook: FAIL"; exit 1; fi
