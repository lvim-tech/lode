#!/bin/sh
# tests/all.sh — every shell suite, each under a ceiling, with its EXIT STATUS.
#
# This exists because two suites lied and neither was noticed by eye.
#
#   * `profile.sh` printed all 26 of its checks and then hung for ever on a bare
#     `wait $PID`: its toy servers had inherited the engine's stdin pipe, so
#     closing it never reached the engine as EOF. A suite that hangs reports
#     NOTHING, and in a sequence it stops everything behind it.
#   * `tunnel.sh` printed "tunnel: PASS" and exited 1. Its cleanup trap ran
#     `[ -n "$PID" ] && kill "$PID"` under `set -e`; the process was already
#     gone, the kill failed, the trap ended early, and the failed kill became
#     the script's status.
#
# Both were invisible to anybody reading the output. So the check is the
# MEASUREMENT: the time it took and the status it exited with, printed for each.
#
#   tests/all.sh            everything
#   tests/all.sh row dash   only those
#
# The engine must be built first: scripts/build.

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LIMIT=${LODE_TEST_TIMEOUT:-180}

# THE BINARY MUST BE NEWER THAN THE SOURCE.
#
# Twice now a result has been read off a build that never happened: an edit was
# applied, the build step was skipped or failed, and the suite exercised the
# previous binary. The output looks exactly like a real run — which is the whole
# problem, because a passing test of the wrong code is worse than no test.
#
# A check, not a habit. Habits are what the last two failures were made of.
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ -x "$BIN" ]; then
    NEWER=$(find "$ROOT/src" "$ROOT/Cargo.toml" -newer "$BIN" -print -quit 2>/dev/null)
    if [ -n "$NEWER" ]; then
        echo "all: STALE — $BIN is older than $NEWER" >&2
        echo "     run scripts/build first; a pass here would be a pass for the previous build" >&2
        exit 2
    fi
else
    echo "all: no engine binary — run scripts/build first" >&2
    exit 2
fi
SUITES=${*:-"smoke row dash hook tunnel profile access mcp driver icons"}
LOGS=$(mktemp -d)
BAD=0

printf '%-10s %-8s %6s  %s\n' SUITE STATUS TIME RESULT
for t in $SUITES; do
    f="$ROOT/tests/$t.sh"
    if [ ! -f "$f" ]; then
        printf '%-10s %-8s %6s  %s\n' "$t" "missing" "-" "no such suite"
        BAD=1
        continue
    fi
    start=$(date +%s)
    if command -v timeout >/dev/null; then
        timeout "$LIMIT" sh "$f" >"$LOGS/$t" 2>&1
        code=$?
    else
        sh "$f" >"$LOGS/$t" 2>&1
        code=$?
    fi
    end=$(date +%s)
    last=$(tail -1 "$LOGS/$t")
    # 124 is what `timeout` exits with when it had to kill the suite. Named,
    # because "exit 124" on its own looks like a test failure and it is not —
    # it is a suite that never finished, which is a different problem.
    case "$code" in
        0) state="ok" ;;
        124) state="TIMEOUT"; last="never finished — ${LIMIT}s ceiling" ;;
        *) state="FAILED" ;;
    esac
    [ "$code" -eq 0 ] || BAD=1
    printf '%-10s %-8s %5ss  %s\n' "$t" "$state" "$((end - start))" "$last"
    # The whole log, but only for the ones that went wrong: a passing suite's
    # output is forty lines nobody reads, and a failing one's is the point.
    if [ "$code" -ne 0 ]; then
        sed 's/^/    /' "$LOGS/$t"
    fi
done

if [ "$BAD" -eq 0 ]; then
    echo "all: PASS"
else
    echo "all: FAIL"
fi
exit $BAD
