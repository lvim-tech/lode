#!/bin/sh
# tests/panels.sh — the numbers every panel is laid out from.
#
# "The settings panel is empty" is not a number, and neither end of this work
# can see the window: it opens on a workspace tag that is not displayed, and a
# full-screen grab catches somebody else's terminal. So the drawing is checked
# through what it is drawn FROM.
#
# Three separate bugs this week were invisible drawings that reached the user
# broken. This is the standing answer to that.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_GUI:-$ROOT/build/lode}
FAIL=0

[ -x "$BIN" ] || { echo "panels: no client binary at $BIN — run scripts/build-gui first"; exit 2; }
NEWER=$(find "$ROOT/crates/lode-ui/src" -newer "$BIN" -print -quit 2>/dev/null)
[ -z "$NEWER" ] || { echo "panels: STALE — $BIN is older than $NEWER"; exit 2; }

OUT=$(timeout 60 "$BIN" --check-panels 2>&1)
CODE=$?
[ "$CODE" -eq 0 ] || { echo "panels: --check-panels exited $CODE"; FAIL=1; }

# 1. Every width and height positive. A zero width draws a border and nothing
#    else, which is exactly what an empty panel with an edge looks like.
if ! printf '%s' "$OUT" | grep -q "geometry: every width and height is positive"; then
    echo "panels: a width or a height is not positive:"
    printf '%s\n' "$OUT" | grep -i "geometry"
    FAIL=1
fi

# 2. The settings panel has its five groups, and one of them is open. A panel
#    with everything shut looks broken; a panel with no groups IS broken.
N=$(printf '%s' "$OUT" | sed -n 's/^settings groups: \([0-9]*\) .*/\1/p')
if [ "$N" != "5" ]; then
    echo "panels: expected 5 settings groups, got '${N:-none}'"
    FAIL=1
fi
if ! printf '%s' "$OUT" | grep -q 'settings groups: .*\*'; then
    echo "panels: no settings group is open — the panel would look shut"
    FAIL=1
fi

if [ "$FAIL" -eq 0 ]; then
    printf '%s\n' "$OUT" | grep -E "^(panel_w|line_h|settings groups)"
    echo "panels: PASS"
    exit 0
fi
echo "panels: FAIL"
exit 1
