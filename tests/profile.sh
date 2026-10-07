#!/bin/sh
# tests/profile.sh — reusable tunnels and proxies, and the one refusal that
# matters most.
#
# A profile is a piece of NETWORK by name: an SSH tunnel, a proxy. The thing
# worth proving is not that one saves and loads — it is that a connection
# naming a profile which is NOT THERE fails, loudly, instead of quietly opening
# a direct link. A connection whose author asked for a bastion and got a plain
# TCP connection to the database is the single worst outcome this code can have,
# and it is the outcome that costs nothing to produce by accident.
#
#   a profile saves, loads, and comes back parsed      not as an opaque string
#   a malformed tunnel is refused ON SAVE              not at three in the morning
#   a connection naming a missing profile FAILS        and never falls back
#   naming a profile AND spelling one out is refused   two tunnels is not a thing
#   removing a profile says who was using it           a surprise is not a choice
#   a real SOCKS5 proxy carries a real connection      end to end, on loopback
#
# Needs no network and no database server: a throwaway SQLite file, `jq`, and a
# toy SOCKS5 proxy written in the shell's reach (python3).
#
#   scripts/build && tests/profile.sh
set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ ! -x "$BIN" ]; then
    echo "profile: no loded binary — run scripts/build first" >&2
    exit 1
fi
command -v jq >/dev/null || { echo "profile: jq is required" >&2; exit 1; }
command -v sqlite3 >/dev/null || { echo "profile: sqlite3 is required" >&2; exit 1; }

WORK=$(mktemp -d)
DB=$WORK/p.sqlite
IN=$WORK/in
OUT=$WORK/out
FAILED=0
PROXY_PID=
trap 'kill $PID $PROXY_PID 2>/dev/null || true' EXIT

sqlite3 "$DB" "CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1),(2);"

mkfifo "$IN"
: > "$OUT"
# LODE_DATA_DIR in the throwaway directory, FIRST, before anything else is
# written here: this test makes the engine save profiles, and a profile from a
# test belongs in a test's database.
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
        printf 'ok   %-48s %s\n' "$label" "$got"
    else
        printf 'FAIL %-48s got %s, want %s\n' "$label" "$got" "$want"
        FAILED=1
    fi
}

mentions() {
    label=$1; line=$2; word=$3
    msg=$(printf '%s' "$line" | jq -r '.error // ""')
    case "$msg" in
        *"$word"*) printf 'ok   %-48s %s\n' "$label" "says \"$word\"" ;;
        *) printf 'FAIL %-48s %s\n' "$label" "message was: $msg"; FAILED=1 ;;
    esac
}

send '{"jsonrpc":"2.0","id":1,"method":"rpc.hello"}'
check "protocol carries profiles" \
    "$(await '.id == 1' | jq '.result.proto >= 9')" "true"

# ── a tunnel profile ─────────────────────────────────────────────────────────
send '{"jsonrpc":"2.0","id":10,"method":"store.profiles.save","params":{"kind":"tunnel","name":"bastion","spec":{"host":"ssh.example","port":2222,"user":"deploy","auth":{"kind":"key","path":"/home/x/.ssh/id_ed25519","passphrase":"{{ vault \"ssh/bastion\" }}"}}}}'
check "a tunnel profile saves" "$(await '.id == 10' | jq '.result.saved')" "true"

send '{"jsonrpc":"2.0","id":11,"method":"store.profiles.list","params":{"kind":"tunnel"}}'
L=$(await '.id == 11')
check "it is listed" "$(printf '%s' "$L" | jq -r '.result.profiles[0].name')" "bastion"
# Parsed, not an opaque string: a client that had to parse it would be a second
# parser of the same shape.
check "its spec comes back as an object" \
    "$(printf '%s' "$L" | jq -r '.result.profiles[0].spec.host')" "ssh.example"
check "the port survived as a number" \
    "$(printf '%s' "$L" | jq '.result.profiles[0].spec.port')" "2222"
# THE TEMPLATE, never a resolved value.
check "the credential is still a template" \
    "$(printf '%s' "$L" | jq -r '.result.profiles[0].spec.auth.passphrase')" '{{ vault "ssh/bastion" }}'

# Parsed on the way IN, so a malformed profile is refused now rather than at
# three in the morning when a connection that names it will not open.
send '{"jsonrpc":"2.0","id":12,"method":"store.profiles.save","params":{"kind":"tunnel","name":"broken","spec":{"port":22}}}'
mentions "a tunnel with no host is refused on save" "$(await '.id == 12')" "not a tunnel"
send '{"jsonrpc":"2.0","id":13,"method":"store.profiles.list","params":{"kind":"tunnel"}}'
check "and nothing broken was stored" "$(await '.id == 13' | jq '.result.profiles | length')" "1"

send '{"jsonrpc":"2.0","id":14,"method":"store.profiles.save","params":{"kind":"wormhole","name":"x","spec":{}}}'
mentions "an unknown kind is refused" "$(await '.id == 14')" "unknown profile kind"

# ── a proxy profile ──────────────────────────────────────────────────────────
send '{"jsonrpc":"2.0","id":20,"method":"store.profiles.save","params":{"kind":"proxy","name":"office","spec":{"kind":"socks5","host":"127.0.0.1","port":1080}}}'
check "a proxy profile saves" "$(await '.id == 20' | jq '.result.saved')" "true"
# Separate namespaces: a tunnel and a proxy may both be called "office" without
# meaning each other.
send '{"jsonrpc":"2.0","id":21,"method":"store.profiles.list","params":{"kind":"proxy"}}'
check "proxies are their own namespace" \
    "$(await '.id == 21' | jq '.result.profiles | length')" "1"
send '{"jsonrpc":"2.0","id":22,"method":"store.profiles.get","params":{"kind":"proxy","name":"bastion"}}'
check "a tunnel name does not find a proxy" "$(await '.id == 22' | jq '.result.spec')" "null"

# ── THE REFUSAL THAT MATTERS ─────────────────────────────────────────────────
# A connection naming a profile that is not there must FAIL. Falling back to a
# direct link would mean somebody asked for a bastion and silently got a plain
# connection to the database.
send "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"conn.connect\",\"params\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"},\"auth\":{\"kind\":\"none\"},\"tunnel_profile\":\"gone\"}}"
L=$(await '.id == 30')
mentions "a missing tunnel profile fails the connect" "$L" "no saved tunnel profile called 'gone'"
mentions "and says it will NOT fall back" "$L" "NOT"

send "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"conn.connect\",\"params\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"},\"auth\":{\"kind\":\"none\"},\"proxy_profile\":\"gone\"}}"
mentions "a missing proxy profile fails too" "$(await '.id == 31')" "no saved proxy profile called 'gone'"

# Two tunnels is not a thing a connection can have, so it is refused rather than
# resolved by precedence — precedence would mean one of them silently losing.
send "{\"jsonrpc\":\"2.0\",\"id\":32,\"method\":\"conn.connect\",\"params\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"},\"auth\":{\"kind\":\"none\"},\"tunnel_profile\":\"bastion\",\"tunnel\":{\"host\":\"other\",\"port\":22,\"user\":\"u\",\"auth\":{\"kind\":\"agent\"}}}}"
mentions "naming a profile AND writing one is refused" "$(await '.id == 32')" "pick one"

# And the same spec through conn.test, because a stage must test what would
# actually be used.
send "{\"jsonrpc\":\"2.0\",\"id\":33,\"method\":\"conn.test\",\"params\":{\"stage\":\"endpoint\",\"spec\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"},\"auth\":{\"kind\":\"none\"},\"tunnel_profile\":\"gone\"}}}"
mentions "conn.test resolves profiles the same way" "$(await '.id == 33')" "no saved tunnel profile"

# A connection with NO profile named still works, which is the control: the
# refusals above are about naming something missing, not about naming nothing.
send "{\"jsonrpc\":\"2.0\",\"id\":34,\"method\":\"conn.connect\",\"params\":{\"driver\":\"sqlite\",\"params\":{\"file\":\"$DB\"},\"auth\":{\"kind\":\"none\"}}}"
check "a connection naming no profile is unaffected" \
    "$(await '.id == 34' | jq '.result.conn_id != null')" "true"

# ── removing a profile says who was using it ─────────────────────────────────
send '{"jsonrpc":"2.0","id":40,"method":"store.connections.save","params":{"name":"prod","driver":"postgres","spec":{"params":{"host":"db"},"tunnel_profile":"bastion"}}}'
await '.id == 40' >/dev/null
send '{"jsonrpc":"2.0","id":41,"method":"store.connections.save","params":{"name":"staging","driver":"postgres","spec":{"params":{"host":"db2"},"tunnel_profile":"bastion"}}}'
await '.id == 41' >/dev/null
send '{"jsonrpc":"2.0","id":42,"method":"store.connections.save","params":{"name":"local","driver":"postgres","spec":{"params":{"host":"localhost"}}}}'
await '.id == 42' >/dev/null

send '{"jsonrpc":"2.0","id":43,"method":"store.profiles.remove","params":{"kind":"tunnel","name":"bastion"}}'
L=$(await '.id == 43')
check "it is removed" "$(printf '%s' "$L" | jq '.result.removed')" "true"
# It IS removed — it is the person's data. But a removal that takes two
# connections down silently is a surprise, not a choice.
check "and names who was using it" \
    "$(printf '%s' "$L" | jq -c '.result.was_used_by | sort')" '["prod","staging"]'
check "a connection that was not using it is not named" \
    "$(printf '%s' "$L" | jq '[.result.was_used_by[] | select(. == "local")] | length')" "0"

# ── a real proxy, carrying a real connection ─────────────────────────────────
# A toy SOCKS5 proxy on loopback. It is twenty lines because the handshake is
# twenty lines, and a test against a mock would be a test of the mock.
if command -v python3 >/dev/null; then
    cat > "$WORK/socks.py" <<'PY'
import socket, struct, threading, sys
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 0)); srv.listen(8)
print(srv.getsockname()[1], flush=True)
def one(c):
    try:
        n = c.recv(2)
        c.recv(n[1]); c.sendall(b"\x05\x00")
        head = c.recv(4)
        ln = c.recv(1)[0]; host = c.recv(ln).decode(); port = struct.unpack("!H", c.recv(2))[0]
        # A real SOCKS5 proxy ANSWERS a request it cannot fulfil: 0x04 is "host
        # unreachable". Dropping the socket instead would be a broken proxy, and
        # the engine is right to call that a failure rather than a verdict.
        try:
            up = socket.create_connection((host, port), 5)
        except Exception:
            c.sendall(b"\x05\x04\x00\x01" + b"\x00" * 6)
            return
        c.sendall(b"\x05\x00\x00\x01" + b"\x00" * 6)
        def pipe(a, b):
            try:
                while True:
                    d = a.recv(4096)
                    if not d: break
                    b.sendall(d)
            except Exception: pass
            try: b.shutdown(socket.SHUT_WR)
            except Exception: pass
        threading.Thread(target=pipe, args=(c, up), daemon=True).start()
        pipe(up, c)
    except Exception: pass
    finally:
        try: c.close()
        except Exception: pass
while True:
    conn, _ = srv.accept()
    threading.Thread(target=one, args=(conn,), daemon=True).start()
PY
    python3 "$WORK/socks.py" > "$WORK/socks.port" 2>"$WORK/socks.err" &
    PROXY_PID=$!
    i=0
    while [ ! -s "$WORK/socks.port" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
    SPORT=$(head -1 "$WORK/socks.port")

    # Something on the far side for the proxy to reach: a listener that answers
    # and closes. The point is that the PROXY got there, not what it found.
    python3 - "$WORK/echo.port" <<'PY' &
import socket, sys, threading
srv = socket.socket(); srv.bind(("127.0.0.1", 0)); srv.listen(4)
open(sys.argv[1], "w").write(str(srv.getsockname()[1]))
while True:
    c, _ = srv.accept()
    threading.Thread(target=lambda s: (s.sendall(b"HI"), s.close()), args=(c,), daemon=True).start()
PY
    ECHO_PID=$!
    i=0
    while [ ! -s "$WORK/echo.port" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
    EPORT=$(head -1 "$WORK/echo.port")

    if [ -n "$SPORT" ] && [ -n "$EPORT" ]; then
        send "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"store.profiles.save\",\"params\":{\"kind\":\"proxy\",\"name\":\"toy\",\"spec\":{\"kind\":\"socks5\",\"host\":\"127.0.0.1\",\"port\":$SPORT}}}"
        await '.id == 50' >/dev/null
        # The endpoint stage THROUGH the proxy. A pass here means the proxy
        # completed its handshake and reached the far listener: end to end.
        send "{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"conn.test\",\"params\":{\"stage\":\"endpoint\",\"spec\":{\"driver\":\"postgres\",\"params\":{\"host\":\"127.0.0.1\",\"port\":\"$EPORT\"},\"auth\":{\"kind\":\"none\"},\"proxy_profile\":\"toy\"}}}"
        L=$(await '.id == 51')
        check "the endpoint is reached THROUGH the proxy" "$(printf '%s' "$L" | jq '.result.ok')" "true"
        check "and the answer says which proxy" \
            "$(printf '%s' "$L" | jq -r '.result.detail | test("SOCKS5 proxy")')" "true"

        # A port with nothing on it, through the SAME proxy: the proxy works and
        # the endpoint does not, and the message must be about the endpoint.
        send "{\"jsonrpc\":\"2.0\",\"id\":52,\"method\":\"conn.test\",\"params\":{\"stage\":\"endpoint\",\"spec\":{\"driver\":\"postgres\",\"params\":{\"host\":\"127.0.0.1\",\"port\":\"9\"},\"auth\":{\"kind\":\"none\"},\"proxy_profile\":\"toy\"}}}"
        L=$(await '.id == 52')
        check "a dead endpoint through a live proxy fails" "$(printf '%s' "$L" | jq '.result.ok // false')" "false"

        # And the proxy STAGE, which asks only about the proxy. It asks for a
        # name that cannot exist on purpose: a proxy that gives a verdict has
        # worked, and asking it for the database would fold two answers into one.
        send "{\"jsonrpc\":\"2.0\",\"id\":53,\"method\":\"conn.test\",\"params\":{\"stage\":\"proxy\",\"spec\":{\"driver\":\"postgres\",\"params\":{\"host\":\"127.0.0.1\"},\"auth\":{\"kind\":\"none\"},\"proxy_profile\":\"toy\"}}}"
        L=$(await '.id == 53')
        check "the proxy stage passes on a working proxy" "$(printf '%s' "$L" | jq '.result.ok')" "true"

        # A proxy that is not there names the PROXY, not the database: the two
        # failures look identical from a driver and send somebody to the wrong
        # machine.
        send '{"jsonrpc":"2.0","id":54,"method":"store.profiles.save","params":{"kind":"proxy","name":"dead","spec":{"kind":"socks5","host":"127.0.0.1","port":1}}}'
        await '.id == 54' >/dev/null
        send "{\"jsonrpc\":\"2.0\",\"id\":55,\"method\":\"conn.test\",\"params\":{\"stage\":\"endpoint\",\"spec\":{\"driver\":\"postgres\",\"params\":{\"host\":\"db.internal\",\"port\":\"5432\"},\"auth\":{\"kind\":\"none\"},\"proxy_profile\":\"dead\"}}}"
        mentions "an absent proxy names the proxy" "$(await '.id == 55')" "cannot reach the proxy"
        kill $ECHO_PID 2>/dev/null || true
    else
        printf '     %-48s %s\n' "toy SOCKS5 proxy did not start" "skipped"
    fi
else
    printf '     %-48s %s\n' "no python3 for the toy proxy" "skipped"
fi

exec 3>&-
wait $PID 2>/dev/null || true
if [ "$FAILED" -eq 0 ]; then echo "profile: PASS"; else echo "profile: FAIL"; exit 1; fi
