#!/bin/sh
# tests/mcp.sh — the engine offered to something that is not a person.
#
# The whole point of this suite is that the MCP server has NO SOFTER PATH. It is
# the same engine, the same `classify`, the same connection policy and the same
# audit log; what differs only ever restricts. So what is tested is the
# restricting:
#
#   a reader refuses every write                     even on an unrestricted connection
#   a read-only connection refuses in its own words  the connection's rule comes first
#   `open` is read as `confirm` for an agent         a person's "stop asking me" is not inherited
#   a dangerous statement is NOT RUN                 it is filed for a person
#   asking again files nothing new                   an agent in a loop is one question
#   a person approves, and it runs ONCE              an approval is spent
#   a denial is final                                and says so
#   every call is in the audit log                   including the refusals
#   the person's decision is in the audit log too    "who let this through"
#
# Needs no network and no model: the MCP protocol is newline JSON on stdio, so
# this drives it the same way the other suites drive the engine.
#
#   scripts/build && tests/mcp.sh
set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ ! -x "$BIN" ]; then
    echo "mcp: no loded binary — run scripts/build first" >&2
    exit 1
fi
command -v jq >/dev/null || { echo "mcp: jq is required" >&2; exit 1; }
command -v sqlite3 >/dev/null || { echo "mcp: sqlite3 is required" >&2; exit 1; }

WORK=$(mktemp -d)
DB=$WORK/m.sqlite
FAILED=0
# LODE_DATA_DIR in the throwaway directory, FIRST: this suite runs TWO engines
# that must share a store — the agent files an approval and the person answers
# it in the other one — and neither belongs in the real store.
DATA=$WORK/data
trap 'kill $MPID $HPID 2>/dev/null || true' EXIT

sqlite3 "$DB" "
    CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT);
    INSERT INTO people(name) VALUES ('Ada'),('Grace'),('Alan');
    -- A second table, so there is a second DANGEROUS statement to approve that
    -- is not the one already denied. `DELETE ... WHERE id = 1` is NOT dangerous
    -- — it names the row it removes — and using it here tested nothing.
    CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT);
    INSERT INTO notes(body) VALUES ('one'),('two');
"

check() {
    label=$1; got=$2; want=$3
    if [ "$got" = "$want" ]; then
        printf 'ok   %-52s %s\n' "$label" "$got"
    else
        printf 'FAIL %-52s got %s, want %s\n' "$label" "$got" "$want"
        FAILED=1
    fi
}

says() {
    label=$1; text=$2; word=$3
    case "$text" in
        *"$word"*) printf 'ok   %-52s %s\n' "$label" "says \"$word\"" ;;
        *) printf 'FAIL %-52s %s\n' "$label" "text was: $(printf '%s' "$text" | head -c 160)"; FAILED=1 ;;
    esac
}

# ── the person's engine, which is where approvals are answered ───────────────
HIN=$WORK/hin; HOUT=$WORK/hout
mkfifo "$HIN"; : > "$HOUT"
LODE_DATA_DIR=$DATA "$BIN" < "$HIN" > "$HOUT" 2>"$WORK/herr" &
HPID=$!
exec 4>"$HIN"
hsend() { printf '%s\n' "$1" >&4; }
hawait() {
    i=0
    while [ "$i" -lt 150 ]; do
        line=$(jq -c --slurp "map(select($1)) | last // empty" "$HOUT" 2>/dev/null || true)
        if [ -n "$line" ] && [ "$line" != "null" ]; then printf '%s\n' "$line"; return 0; fi
        sleep 0.1; i=$((i + 1))
    done
    echo "TIMED OUT (human) waiting for $1" >&2; return 1
}

# A saved connection for the agent to open, and an UNRESTRICTED one — the policy
# a person sets for themselves, which an agent must not inherit.
hsend "$(jq -nc --arg f "$DB" '{jsonrpc:"2.0",id:1,method:"store.connections.save",params:{name:"scratch",driver:"sqlite",spec:{params:{file:$f},auth:{kind:"none"},policy:"open"}}}')"
hawait '.id == 1' >/dev/null
hsend "$(jq -nc --arg f "$DB" '{jsonrpc:"2.0",id:2,method:"store.connections.save",params:{name:"readonly",driver:"sqlite",spec:{params:{file:$f},auth:{kind:"none"},policy:"read_only"}}}')"
hawait '.id == 2' >/dev/null

# ── the agent's engine ───────────────────────────────────────────────────────
start_mcp() {
    role=$1
    [ -n "$MPID" ] && { kill $MPID 2>/dev/null || true; }
    MIN=$WORK/min.$role; MOUT=$WORK/mout.$role
    rm -f "$MIN" 2>/dev/null || true
    mkfifo "$MIN"; : > "$MOUT"
    LODE_DATA_DIR=$DATA LODE_MCP_ROLE=$role "$BIN" --mcp < "$MIN" > "$MOUT" 2>>"$WORK/merr" &
    MPID=$!
    exec 3>"$MIN"
    msend '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
    mawait '.id == 1' >/dev/null
}
msend() { printf '%s\n' "$1" >&3; }
mawait() {
    i=0
    while [ "$i" -lt 200 ]; do
        line=$(jq -c --slurp "map(select($1)) | last // empty" "$MOUT" 2>/dev/null || true)
        if [ -n "$line" ] && [ "$line" != "null" ]; then printf '%s\n' "$line"; return 0; fi
        sleep 0.1; i=$((i + 1))
    done
    echo "TIMED OUT (mcp) waiting for $1" >&2; return 1
}
# The text a tool returned, and whether it said no.
tool() {
    id=$1; name=$2; args=$3
    msend "$(jq -nc --argjson i "$id" --arg n "$name" --argjson a "$args" \
        '{jsonrpc:"2.0",id:$i,method:"tools/call",params:{name:$n,arguments:$a}}')"
    mawait ".id == $id"
}
text_of() { printf '%s' "$1" | jq -r '.result.content[0].text'; }
is_error() { printf '%s' "$1" | jq '.result.isError // false'; }

MPID=
start_mcp reader

# ── the handshake ────────────────────────────────────────────────────────────
msend '{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}'
H=$(mawait '.id == 2')
check "it speaks a protocol version" "$(printf '%s' "$H" | jq -r '.result.protocolVersion')" "2024-11-05"
check "it names itself" "$(printf '%s' "$H" | jq -r '.result.serverInfo.name')" "lode"
says "the handshake states the role" "$(printf '%s' "$H" | jq -r '.result.instructions')" "role is 'reader'"
says "and that a person is in the loop" "$(printf '%s' "$H" | jq -r '.result.instructions')" "filed for a person"

# A notification gets no answer at all.
BEFORE=$(wc -l < "$MOUT")
msend '{"jsonrpc":"2.0","method":"notifications/initialized"}'
sleep 0.5
check "a notification is not answered" "$(wc -l < "$MOUT")" "$BEFORE"

msend '{"jsonrpc":"2.0","id":3,"method":"tools/list"}'
T=$(mawait '.id == 3')
check "it lists its tools" "$(printf '%s' "$T" | jq '.result.tools | length >= 5')" "true"
check "and each one has a schema" \
    "$(printf '%s' "$T" | jq '[.result.tools[] | select(.inputSchema.type != "object")] | length')" "0"

# ── what a reader can do ─────────────────────────────────────────────────────
L=$(tool 10 list_connections '{}')
says "it can list connections" "$(text_of "$L")" "scratch"
# Names and drivers only. The spec holds hosts, tunnels and credential
# templates, and none of that is the agent's.
check "and NOT their specs" "$(printf '%s' "$L" | jq -r '.result.content[0].text | test("file|params|auth")')" "false"

L=$(tool 11 open '{"name":"scratch"}')
says "it can open one" "$(text_of "$L")" "conn_id"
CID=$(text_of "$L" | grep -o '[0-9]*$')

L=$(tool 12 query "{\"conn_id\":$CID,\"statement\":\"SELECT name FROM people ORDER BY id\"}")
check "a read runs" "$(is_error "$L")" "false"
says "and returns the rows" "$(text_of "$L")" "Grace"
says "with a count" "$(text_of "$L")" "row(s) shown"

L=$(tool 13 open '{"name":"nope"}')
check "opening something that is not there fails" "$(is_error "$L")" "true"
says "and says the name" "$(text_of "$L")" "'nope'"

# ── a reader cannot write, even on an UNRESTRICTED connection ────────────────
# The role is a floor, not a key.
L=$(tool 20 query "{\"conn_id\":$CID,\"statement\":\"INSERT INTO people(name) VALUES ('Mallory')\"}")
check "a reader's INSERT is refused" "$(is_error "$L")" "true"
says "and says it is because it is a reader" "$(text_of "$L")" "is a reader"
check "and nothing was inserted" "$(sqlite3 "$DB" "SELECT count(*) FROM people")" "3"

L=$(tool 21 query "{\"conn_id\":$CID,\"statement\":\"DROP TABLE people\"}")
check "a reader's DROP is refused too" "$(is_error "$L")" "true"
check "and the table is still there" "$(sqlite3 "$DB" "SELECT count(*) FROM people")" "3"

# ── a writer, on a read-only CONNECTION ──────────────────────────────────────
start_mcp writer
L=$(tool 30 open '{"name":"readonly"}')
RID=$(text_of "$L" | grep -o '[0-9]*$')
L=$(tool 31 query "{\"conn_id\":$RID,\"statement\":\"INSERT INTO people(name) VALUES ('Mallory')\"}")
check "a writer still obeys a read-only connection" "$(is_error "$L")" "true"
# The CONNECTION's own words, not the agent's: whose rule stopped it matters.
says "and it is the connection that says so" "$(text_of "$L")" "connection is read only"

# ── a writer, on an UNRESTRICTED connection ──────────────────────────────────
# "Unrestricted" was a decision a person made about THEMSELVES. It means "stop
# asking me", not "let anything that can speak JSON-RPC drop a table".
L=$(tool 40 open '{"name":"scratch"}')
WID=$(text_of "$L" | grep -o '[0-9]*$')
L=$(tool 41 query "{\"conn_id\":$WID,\"statement\":\"INSERT INTO people(name) VALUES ('Bob')\"}")
check "an ordinary write goes through" "$(is_error "$L")" "false"
check "and it really wrote" "$(sqlite3 "$DB" "SELECT count(*) FROM people WHERE name='Bob'")" "1"

L=$(tool 42 query "{\"conn_id\":$WID,\"statement\":\"DELETE FROM people\"}")
check "a DANGEROUS statement is refused" "$(is_error "$L")" "true"
says "it says it was NOT RUN" "$(text_of "$L")" "NOT RUN"
says "it says why it is dangerous" "$(text_of "$L")" "EVERY row"
says "it says a person has it" "$(text_of "$L")" "needs a person"
check "and nothing was deleted" "$(sqlite3 "$DB" "SELECT count(*) FROM people")" "4"

# Asking again files NOTHING NEW. An agent retrying in a loop must not fill a
# person's queue with one question.
tool 43 query "{\"conn_id\":$WID,\"statement\":\"DELETE FROM people\"}" >/dev/null
tool 44 query "{\"conn_id\":$WID,\"statement\":\"DELETE FROM people\"}" >/dev/null
hsend '{"jsonrpc":"2.0","id":10,"method":"approvals.list","params":{"state":"pending"}}'
A=$(hawait '.id == 10')
check "asking three times files ONE approval" \
    "$(printf '%s' "$A" | jq '[.result.approvals[] | select(.statement | test("DELETE FROM people"))] | length')" "1"
check "the person sees what was asked" \
    "$(printf '%s' "$A" | jq -r '.result.approvals[0].asker')" "mcp"
check "and why it needed asking" \
    "$(printf '%s' "$A" | jq -r '.result.approvals[0].reason | test("EVERY row")')" "true"
AID=$(printf '%s' "$A" | jq '.result.approvals[0].id')

# ── a person says no ─────────────────────────────────────────────────────────
hsend "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"approvals.decide\",\"params\":{\"id\":$AID,\"approve\":false}}"
check "a person can deny it" "$(hawait '.id == 11' | jq '.result.decided')" "true"
L=$(tool 45 query "{\"conn_id\":$WID,\"statement\":\"DELETE FROM people\"}")
check "and the agent still cannot run it" "$(is_error "$L")" "true"
# A denial STANDS. Without this, "no" means "not this second": the agent asks
# again, a fresh pending row appears, and the person answers for ever.
says "and it says a person already refused it" "$(text_of "$L")" "already refused"
check "and nothing was deleted" "$(sqlite3 "$DB" "SELECT count(*) FROM people")" "4"
# Deciding twice does not re-open a decision.
hsend "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"approvals.decide\",\"params\":{\"id\":$AID,\"approve\":true}}"
check "a decided approval cannot be re-decided" "$(hawait '.id == 12' | jq '.result.decided')" "false"

# ── a person says yes ────────────────────────────────────────────────────────
hsend '{"jsonrpc":"2.0","id":20,"method":"approvals.list","params":{"state":"pending"}}'
A=$(hawait '.id == 20')
check "re-asking a denied one filed nothing new" \
    "$(printf '%s' "$A" | jq '[.result.approvals[] | select(.statement == "DELETE FROM people")] | length')" "0"

L=$(tool 50 query "{\"conn_id\":$WID,\"statement\":\"DELETE FROM notes\"}")
check "a second dangerous statement is also filed" "$(is_error "$L")" "true"
hsend '{"jsonrpc":"2.0","id":21,"method":"approvals.list","params":{"state":"pending"}}'
BID=$(hawait '.id == 21' | jq '[.result.approvals[] | select(.statement == "DELETE FROM notes")][0].id')
hsend "{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"approvals.decide\",\"params\":{\"id\":$BID,\"approve\":true}}"
check "a person can approve it" "$(hawait '.id == 22' | jq '.result.decided')" "true"

L=$(tool 51 query "{\"conn_id\":$WID,\"statement\":\"DELETE FROM notes\"}")
check "and now it runs" "$(is_error "$L")" "false"
check "and it really deleted" "$(sqlite3 "$DB" "SELECT count(*) FROM notes")" "0"

# SPENT. They said yes to a DELETE, not to every DELETE from now on.
sqlite3 "$DB" "INSERT INTO notes(body) VALUES ('three')"
L=$(tool 52 query "{\"conn_id\":$WID,\"statement\":\"DELETE FROM notes\"}")
check "the same statement again is refused" "$(is_error "$L")" "true"
says "because the approval was spent" "$(text_of "$L")" "NOT RUN"
check "and the row is still there" "$(sqlite3 "$DB" "SELECT count(*) FROM notes")" "1"

# ── all of it is in the audit log ────────────────────────────────────────────
hsend '{"jsonrpc":"2.0","id":30,"method":"store.audit.list","params":{"limit":500}}'
AU=$(hawait '.id == 30')
aq() { printf '%s' "$AU" | jq "$@"; }
check "the agent's writes are audited" \
    "$(aq '[.result.audit[] | select(.path == "mcp.query")] | length > 0')" "true"
check "its REFUSALS are audited" \
    "$(aq '[.result.audit[] | select(.path == "mcp.query" and .outcome == "refused")] | length > 0')" "true"
check "the refusal that was about the role is in it" \
    "$(aq '[.result.audit[] | select(.reason != null and (.reason | test("is a reader")))] | length > 0')" "true"
check "an applied agent write is audited" \
    "$(aq '[.result.audit[] | select(.path == "mcp.query" and .outcome == "applied")] | length > 0')" "true"
# "Who let this through" is the question asked straight after "what ran".
check "the person's decision is audited" \
    "$(aq '[.result.audit[] | select(.path == "approval")] | length >= 2')" "true"
check "and both answers are there" \
    "$(aq '[.result.audit[] | select(.path == "approval") | .outcome] | sort | unique | join(",")')" '"approved,denied"'
# A read is not a change, here as everywhere.
check "the agent's SELECT is not in the log of changes" \
    "$(aq '[.result.audit[] | select(.statement | test("^SELECT"))] | length')" "0"

exec 3>&- 4>&-
kill $MPID $HPID 2>/dev/null || true
i=0
while { kill -0 $MPID 2>/dev/null || kill -0 $HPID 2>/dev/null; } && [ $i -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
if [ "$FAILED" -eq 0 ]; then echo "mcp: PASS"; else echo "mcp: FAIL"; exit 1; fi
exit 0
