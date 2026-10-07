#!/bin/sh
# tests/access.sh — reading somebody's AWS files, and refusing the half that
# needs the network.
#
# The engine offers what `~/.aws/config` and `~/.aws/credentials` describe so
# that nobody has to paste an access key into a connection form. What is worth
# proving:
#
#   the two files name sections differently and merge to ONE list
#   every entry says what KIND it is and whether it can be used
#   SSO and AssumeRole are refused BY NAME with what to do instead
#   a credential_process is run and its JSON read
#   a test says whether it worked, and NOT what it produced
#   an import stores the NAME, never the key
#   importing an unusable profile is refused on the way IN
#
# Needs no network and no AWS: fixture files in a temporary directory, pointed
# at with the same environment variables the AWS tools themselves honour.
#
#   scripts/build && tests/access.sh
set -e

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${LODE_DAEMON:-$ROOT/build/loded}
[ -x "$BIN" ] || BIN=$ROOT/target/release/loded
if [ ! -x "$BIN" ]; then
    echo "access: no loded binary — run scripts/build first" >&2
    exit 1
fi
command -v jq >/dev/null || { echo "access: jq is required" >&2; exit 1; }

WORK=$(mktemp -d)
IN=$WORK/in
OUT=$WORK/out
FAILED=0
trap 'kill $PID 2>/dev/null || true' EXIT

# ── the fixture ──────────────────────────────────────────────────────────────
mkdir -p "$WORK/aws"
cat > "$WORK/aws/config" <<'CFG'
# a comment that mentions [profile ghost] and must not create one
[default]
region = eu-central-1
output = json

[profile dev]
region = eu-west-1
credential_process = printf '{"Version":1,"AccessKeyId":"AKIAPROCESS9999","SecretAccessKey":"s3cr3t","SessionToken":"tok"}'

[profile prod]
region = us-east-1
role_arn = arn:aws:iam::123456789012:role/Admin
source_profile = dev

[profile corp]
sso_session = corp
sso_account_id = 123456789012
sso_role_name = ReadOnly

[profile inherits]
source_profile = default
CFG

cat > "$WORK/aws/credentials" <<'CRD'
[default]
aws_access_key_id = AKIADEFAULT1234
aws_secret_access_key = secret-default

[keyed]
aws_access_key_id = AKIAKEYED5678
aws_secret_access_key = secret-keyed
aws_session_token = tok-keyed
CRD

mkfifo "$IN"
: > "$OUT"
# LODE_DATA_DIR in the throwaway directory, FIRST: this test imports profiles,
# and an imported profile from a test belongs in a test's database.
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

# Every call names the fixture files, so this never reads the person's own.
A="\"config\":\"$WORK/aws/config\",\"credentials\":\"$WORK/aws/credentials\""

send '{"jsonrpc":"2.0","id":1,"method":"rpc.hello"}'
check "protocol carries access providers" \
    "$(await '.id == 1' | jq '.result.proto >= 11')" "true"

send "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"access.list\",\"params\":{$A}}"
L=$(await '.id == 10')
lq() { printf '%s' "$L" | jq "$@"; }

# `[profile dev]` in one file is `[dev]` in the other, and `[default]` is itself
# in both. Getting that wrong gives a list with every name doubled.
check "default appears exactly once" "$(lq '[.result.profiles[] | select(.name=="default")] | length')" "1"
check "a profile from each file is there" \
    "$(lq '[.result.profiles[] | select(.name=="dev" or .name=="keyed")] | length')" "2"
# A comment is not a section.
check "a commented-out profile is not a profile" \
    "$(lq '[.result.profiles[] | select(.name=="ghost")] | length')" "0"
# Where it looked, so an empty list is a fact rather than a mystery.
check "it says which files it read" "$(lq -r '.result.config | test("aws/config")')" "true"

# Each entry says WHAT IT IS, so a client marks the ones that will not work
# rather than offering them and failing when they are picked.
kind() { lq -r "[.result.profiles[] | select(.name==\"$1\")][0].kind"; }
check "static keys are 'static'" "$(kind default)" "static"
check "a credential_process is 'process'" "$(kind dev)" "process"
check "an SSO profile is 'sso'" "$(kind corp)" "sso"
check "a role profile is 'assume_role'" "$(kind prod)" "assume_role"
check "a bare source_profile is 'inherited'" "$(kind inherits)" "inherited"
check "and the unusable ones say so" \
    "$(lq '[.result.profiles[] | select(.usable == false) | .name] | sort | join(",")')" '"corp,prod"'
# The region comes from the config file even when the keys come from the other.
check "the region survives the merge" \
    "$(lq -r '[.result.profiles[] | select(.name=="default")][0].region')" "eu-central-1"

# ── resolving ────────────────────────────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"access.test\",\"params\":{\"profile\":\"keyed\",$A}}"
T=$(await '.id == 20')
check "static keys resolve" "$(printf '%s' "$T" | jq '.result.ok')" "true"
# ENOUGH to tell two profiles apart, not enough to be a credential.
check "and the answer is only the last four" "$(printf '%s' "$T" | jq -r '.result.key_ends_with')" "5678"
check "it says there is a session token" "$(printf '%s' "$T" | jq '.result.has_session_token')" "true"
# THE one that matters: a test button must not hand a key back to be screenshotted.
check "the whole key is NOT in the answer" \
    "$(printf '%s' "$T" | jq -r 'tostring | test("AKIAKEYED5678")')" "false"
check "nor is the secret" \
    "$(printf '%s' "$T" | jq -r 'tostring | test("secret-keyed")')" "false"

# A credential helper is a real program, run for real.
send "{\"jsonrpc\":\"2.0\",\"id\":21,\"method\":\"access.test\",\"params\":{\"profile\":\"dev\",$A}}"
T=$(await '.id == 21')
check "a credential_process is run" "$(printf '%s' "$T" | jq '.result.ok')" "true"
check "and its key is used" "$(printf '%s' "$T" | jq -r '.result.key_ends_with')" "9999"

# source_profile with no role is an inheritance.
send "{\"jsonrpc\":\"2.0\",\"id\":22,\"method\":\"access.test\",\"params\":{\"profile\":\"inherits\",$A}}"
check "a bare source_profile inherits the parent's keys" \
    "$(await '.id == 22' | jq -r '.result.key_ends_with')" "1234"

# ── the two that need the network ────────────────────────────────────────────
# Refused BY NAME with what would have to happen. A silent "no credentials"
# would read as a missing profile rather than an unsupported one.
send "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"access.test\",\"params\":{\"profile\":\"corp\",$A}}"
L2=$(await '.id == 30')
mentions "SSO is refused" "$L2" "SSO"
mentions "and says what to run instead" "$L2" "aws sso login"
send "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"access.test\",\"params\":{\"profile\":\"prod\",$A}}"
L2=$(await '.id == 31')
mentions "AssumeRole is refused" "$L2" "STS"
mentions "and names the role it would have taken" "$L2" "role/Admin"
send "{\"jsonrpc\":\"2.0\",\"id\":32,\"method\":\"access.test\",\"params\":{\"profile\":\"nope\",$A}}"
mentions "a profile that is not there says its name" "$(await '.id == 32')" "'nope'"

# ── importing ────────────────────────────────────────────────────────────────
send "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"access.import\",\"params\":{\"profile\":\"keyed\",\"as_name\":\"billing\",$A}}"
check "an import saves" "$(await '.id == 40' | jq '.result.saved')" "true"
send '{"jsonrpc":"2.0","id":41,"method":"store.profiles.list","params":{"kind":"access"}}'
L3=$(await '.id == 41')
check "it is listed as an access profile" "$(printf '%s' "$L3" | jq -r '.result.profiles[0].name')" "billing"
# BY NAME, never by value: the credential stays where the AWS tools rotate it.
check "it stored the AWS profile's NAME" \
    "$(printf '%s' "$L3" | jq -r '.result.profiles[0].spec.profile')" "keyed"
check "and NOT the key" \
    "$(printf '%s' "$L3" | jq -r 'tostring | test("AKIA")')" "false"
check "nor the secret" \
    "$(printf '%s' "$L3" | jq -r 'tostring | test("secret-keyed")')" "false"

# Refused on the way IN. Importing something that cannot produce credentials
# makes a profile that fails at every connect for a reason stored elsewhere.
send "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"access.import\",\"params\":{\"profile\":\"corp\",$A}}"
L2=$(await '.id == 42')
mentions "importing an SSO profile is refused" "$L2" "cannot get credentials"
mentions "and says what kind it was" "$L2" "'sso'"
send '{"jsonrpc":"2.0","id":43,"method":"store.profiles.list","params":{"kind":"access"}}'
check "and nothing unusable was stored" \
    "$(await '.id == 43' | jq '.result.profiles | length')" "1"

# A fourth kind does not quietly become a namespace.
send '{"jsonrpc":"2.0","id":44,"method":"store.profiles.list","params":{"kind":"wormhole"}}'
mentions "an unknown profile kind is still refused" "$(await '.id == 44')" "'access'"

exec 3>&-
i=0
while kill -0 $PID 2>/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
kill $PID 2>/dev/null || true
if [ "$FAILED" -eq 0 ]; then echo "access: PASS"; else echo "access: FAIL"; exit 1; fi
exit 0
