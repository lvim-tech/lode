#!/bin/sh
# tests/tunnel.sh — the endpoint stage must tell the truth THROUGH an SSH tunnel.
#
# This exists because it did not. `conn.test`'s endpoint stage used to resolve
# the address through `NetContext` and connect to what came back; on a tunnelled
# spec that is the LOCAL forward's listener, which this process had just bound,
# so it answered yes the instant the SSH session came up. A MongoDB connection
# aimed at 127.0.0.1:9042 with nothing listening reported
#
#     ✓ 127.0.0.1:9042 reachable through the SSH tunnel (local 127.0.0.1:42665)
#
# in 251 ms. Green meant "the SSH session is up", not "a database answered".
#
# The test stands up its OWN sshd — its own host key, its own authorized_keys,
# its own config, on its own port, all inside a mktemp directory. It touches
# nothing in ~ and nothing in /etc, and it needs no network: everything is
# 127.0.0.1. It skips cleanly when sshd or ssh-keygen is not installed.
#
#   scripts/build && tests/tunnel.sh
#
# Three questions, which is the point of having stages at all:
#   1. tunnelled endpoint, something listening     → PASS
#   2. tunnelled endpoint, nothing listening       → FAIL  (the bug)
#   3. the tunnel stage itself, in case 2          → PASS  (the tunnel is fine;
#                                                    it is the endpoint that is not)
set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ ! -x "$BIN" ]; then
    echo "tunnel: no loded binary — run scripts/build first" >&2
    exit 1
fi
command -v jq >/dev/null || { echo "tunnel: jq is required" >&2; exit 1; }

SSHD=$(command -v sshd || echo /usr/sbin/sshd)
if [ ! -x "$SSHD" ] || ! command -v ssh-keygen >/dev/null; then
    echo "tunnel: skipped — needs sshd and ssh-keygen"
    exit 0
fi

WORK=$(mktemp -d)
SSHD_PID=""
ENGINE_PID=""
# Every line ends in `|| true`, and that is not belt and braces. This runs
# under `set -e`, and `[ -n x ] && kill x` returns non-zero when the process has
# already gone — which ends the TRAP early, before `return 0`, and the failed
# kill becomes the script's exit status. The suite printed "tunnel: PASS" and
# exited 1, which is the worst of both: a sequence stops and the reason reads
# like a passing test.
cleanup() {
    [ -n "$SSHD_PID" ] && kill "$SSHD_PID" 2>/dev/null || true
    [ -n "$ENGINE_PID" ] && kill "$ENGINE_PID" 2>/dev/null || true
    return 0
}
trap cleanup EXIT

USER_NAME=$(id -un)
SSH_PORT=${LODE_TEST_SSH_PORT:-22222}

ssh-keygen -q -t ed25519 -N "" -f "$WORK/hostkey"
ssh-keygen -q -t ed25519 -N "" -f "$WORK/userkey"
cp "$WORK/userkey.pub" "$WORK/authorized_keys"
chmod 600 "$WORK/authorized_keys" "$WORK/hostkey" "$WORK/userkey"

cat >"$WORK/sshd_config" <<CFG
Port $SSH_PORT
ListenAddress 127.0.0.1
HostKey $WORK/hostkey
PidFile $WORK/sshd.pid
AuthorizedKeysFile $WORK/authorized_keys
StrictModes no
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AllowTcpForwarding yes
PermitTTY no
X11Forwarding no
PrintMotd no
CFG

# sshd insists on an absolute path for itself.
"$SSHD" -f "$WORK/sshd_config" -D -e >"$WORK/sshd.log" 2>&1 &
SSHD_PID=$!
i=0
while [ "$i" -lt 50 ]; do
    if command -v ss >/dev/null && ss -ltn 2>/dev/null | grep -q "127.0.0.1:$SSH_PORT"; then break; fi
    sleep 0.1
    i=$((i + 1))
done
if ! kill -0 "$SSHD_PID" 2>/dev/null; then
    echo "tunnel: skipped — the private sshd would not start:"
    sed 's/^/       /' "$WORK/sshd.log"
    exit 0
fi

# A port with NOTHING on it. Bound and released, so it is free at this instant;
# a port that something grabs in the gap would make this test wrong rather than
# flaky, which is why the positive case below is checked too.
DEAD_PORT=$(
    env python3 - <<'PY' 2>/dev/null || echo 9042
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)

IN=$WORK/in
OUT=$WORK/out
mkfifo "$IN"
: >"$OUT"
LODE_DATA_DIR=$WORK/data "$BIN" <"$IN" >"$OUT" 2>"$WORK/err" &
ENGINE_PID=$!
exec 3>"$IN"

FAILED=0
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
    # Printed, not silent: with `set -e` a bare non-zero return ends the script
    # with no output at all, which looks like a pass that stopped early.
    printf '{"ok":false,"error":"TIMED OUT waiting for %s"}\n' "$filter"
    return 0
}

check() {
    if [ "$2" = "$3" ]; then
        printf 'ok   %-40s %s\n' "$1" "$2"
    else
        printf 'FAIL %-40s got %s, want %s\n' "$1" "$2" "$3"
        FAILED=1
    fi
}
report() { printf '     %-40s %s\n' "$1" "$2"; }

# The spec the form produces for a tunnelled connection, with `TARGET` left to
# be substituted. mongodb, because that is what the report was about.
# ONE LINE: the protocol is newline-delimited JSON, so a pretty-printed request
# is two malformed ones.
#
# `spec` is mongodb, because that is the driver the bug was reported against.
# `spec_pg` is the same tunnel aimed at postgres, for the stage that needs a
# driver which gives up quickly.
spec() {
    printf '%s' "{\"driver\":\"mongodb\",\"params\":{\"host\":\"127.0.0.1\",\"port\":\"$1\",\"database\":\"probe\"},\"auth\":{\"kind\":\"none\"},\"tls\":{\"mode\":\"prefer\"},\"tunnel\":{\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"$USER_NAME\",\"auth\":{\"kind\":\"key\",\"path\":\"$WORK/userkey\",\"passphrase\":\"\"}}}"
}

spec_pg() {
    printf '%s' "{\"driver\":\"postgres\",\"params\":{\"host\":\"127.0.0.1\",\"port\":\"$1\",\"database\":\"probe\"},\"auth\":{\"kind\":\"password\",\"user\":\"probe\",\"password\":\"x\"},\"tls\":{\"mode\":\"prefer\"},\"tunnel\":{\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"$USER_NAME\",\"auth\":{\"kind\":\"key\",\"path\":\"$WORK/userkey\",\"passphrase\":\"\"}}}"
}

send '{"id":1,"method":"rpc.hello","params":{}}'
check "engine answers" "$(await '.id == 1' | jq -r '.result.engine')" "lode"
report "private sshd" "127.0.0.1:$SSH_PORT as $USER_NAME, key auth"

# ── 1. something IS listening on the far side ────────────────────────────────
# The sshd's own port is a listener, which saves standing up a second process.
send "{\"id\":2,\"method\":\"conn.test\",\"params\":{\"stage\":\"endpoint\",\"spec\":$(spec "$SSH_PORT")}}"
LIVE=$(await '.id == 2')
check "endpoint, far side listening" "$(printf '%s' "$LIVE" | jq -r '.ok')" "true"
report "its detail" "$(printf '%s' "$LIVE" | jq -r '.result.detail // .error' | cut -c1-100)"

# ── 2. nothing is listening — THE BUG ────────────────────────────────────────
send "{\"id\":3,\"method\":\"conn.test\",\"params\":{\"stage\":\"endpoint\",\"spec\":$(spec "$DEAD_PORT")}}"
DEAD=$(await '.id == 3')
check "endpoint, far side DEAD (was green)" "$(printf '%s' "$DEAD" | jq -r '.ok')" "false"
report "port probed" "127.0.0.1:$DEAD_PORT"
report "its error" "$(printf '%s' "$DEAD" | jq -r '.error' | cut -c1-110)"
# The message has to name the endpoint, or it is not an endpoint failure.
check "error names the dead port" \
    "$(printf '%s' "$DEAD" | jq -r --arg p "$DEAD_PORT" '.error | test($p)')" "true"

# ── 3. the tunnel stage still passes ─────────────────────────────────────────
# Attribution is the whole reason the stages exist: the tunnel is fine, and only
# the endpoint is not. A tunnel stage that failed here would move the blame.
send "{\"id\":4,\"method\":\"conn.test\",\"params\":{\"stage\":\"tunnel\",\"spec\":$(spec "$DEAD_PORT")}}"
TUN=$(await '.id == 4')
check "tunnel stage, same dead endpoint" "$(printf '%s' "$TUN" | jq -r '.ok')" "true"
report "its detail" "$(printf '%s' "$TUN" | jq -r '.result.detail // .error' | cut -c1-100)"

# ── 4. the deeper stages were never fooled, and must stay that way ───────────
# `tls` and `auth` do a REAL driver connect, so they were correct even while the
# endpoint stage was not: the driver dials the local forward, the channel open
# fails behind it, the socket closes and the handshake dies. Asserted rather than
# assumed, because "it should be fine" is how the endpoint stage got here.
# postgres rather than mongodb for this one: the mongo driver spends its own
# server-selection timeout (tens of seconds) before giving up, which outlasts the
# wait here and turns a real answer into a timeout. postgres fails as soon as the
# socket closes, and the question — "is the deeper stage fooled by the forward?" —
# is the same whichever driver asks it.
send "{\"id\":5,\"method\":\"conn.test\",\"params\":{\"stage\":\"tls\",\"spec\":$(spec_pg "$DEAD_PORT")}}"
TLS=$(await '.id == 5')
check "tls stage, far side DEAD" "$(printf '%s' "$TLS" | jq -r '.ok')" "false"
report "its error" "$(printf '%s' "$TLS" | jq -r '.error' | cut -c1-110)"
# …and for the RIGHT reason. A spec rejected for a missing field would make this
# assertion pass while proving nothing about the tunnel at all, which is the trap
# the first version of this check fell into.
check "tls failed on the LINK, not the spec" \
    "$(printf '%s' "$TLS" | jq -r '.error | test("missing|unknown field|invalid type|TIMED OUT") | not')" "true"

exec 3>&-
sleep 0.5

[ -s "$WORK/err" ] && { echo "--- engine stderr ---"; cat "$WORK/err"; }

if [ "$FAILED" -eq 0 ]; then
    echo "tunnel: PASS"
else
    echo "tunnel: FAIL"
fi
exit $FAILED
