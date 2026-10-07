#!/bin/sh
# tests/icons.sh — the interface's glyphs exist in the font that was chosen.
#
# This is here because of what went wrong: the font picker asked for six
# families BY NAME, none of those names was installed, and it fell through to a
# font with no Font Awesome block. Six icons in the rail drew as empty
# rectangles, and an empty rectangle is reported as "the icons are different
# sizes" rather than as "the icons are missing". The name of a font says nothing
# about what is in it, so the only honest check is to ask.
#
# It runs in a PIPE on purpose. `--check-icons` used to sit there for ever after
# printing — `cx.quit()` does not end the run loop with no window open — which
# down a terminal looks fine and down a pipe looks like a hang. A diagnostic
# that hangs in a pipeline is one nobody puts in a test, which is how it would
# have stayed broken.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_GUI:-$ROOT/build/lode}
FAIL=0

if [ ! -x "$BIN" ]; then
    echo "icons: no client binary at $BIN — run scripts/build-gui first"
    exit 2
fi

# The client's own sources must not be newer than the binary, for the same
# reason the engine's must not be: a pass for the previous build is worse than
# no pass. `scripts/build-gui` builds this one; `scripts/build` does not.
NEWER=$(find "$ROOT/crates/lode-ui/src" -newer "$BIN" -print -quit 2>/dev/null)
if [ -n "$NEWER" ]; then
    echo "icons: STALE — $BIN is older than $NEWER"
    echo "       run scripts/build-gui first"
    exit 2
fi

OUT=$(timeout 60 "$BIN" --check-icons 2>&1)
CODE=$?

# 1. It ended by itself, in a pipe, within the ceiling.
if [ "$CODE" -ne 0 ]; then
    echo "icons: --check-icons exited $CODE (124 means it never finished)"
    FAIL=1
fi

# 2. Every glyph the interface draws is in the font it will draw them with.
if echo "$OUT" | grep -q "MISSING"; then
    echo "icons: the chosen font is missing glyphs:"
    echo "$OUT" | grep "MISSING"
    FAIL=1
fi

# 3. They are one cell wide, which is what makes them one optical size.
RANGE=$(echo "$OUT" | sed -n 's/^advance width: \([0-9.]*\) \.\. \([0-9.]*\).*/\1 \2/p')
LO=$(echo "$RANGE" | cut -d' ' -f1)
HI=$(echo "$RANGE" | cut -d' ' -f2)
if [ -z "$LO" ] || [ "$LO" != "$HI" ]; then
    echo "icons: advance widths differ ($LO .. $HI) — the icons will not look the same size"
    FAIL=1
fi

if [ "$FAIL" -eq 0 ]; then
    echo "$OUT" | grep -E "^(text|icon) font:|glyphs present"
    echo "icons: PASS"
    exit 0
fi
echo "icons: FAIL"
exit 1
