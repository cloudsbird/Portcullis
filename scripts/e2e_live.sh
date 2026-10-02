#!/usr/bin/env bash
# Live end-to-end test against a REAL OpenAI-compatible provider.
#
# The test suite uses mock upstreams. This script exercises what a mock cannot:
# real TLS, real authentication, real SSE framing and real error shapes.
#
# It is opt-in because it spends real money (fractions of a cent) and sends
# traffic to a third party:
#
#   PORTCULLIS_E2E_UPSTREAM_URL=https://api.openai.com/v1/chat/completions \
#   PORTCULLIS_E2E_UPSTREAM_KEY=sk-... \
#   PORTCULLIS_E2E_MODEL=gpt-4o-mini \
#     scripts/e2e_live.sh
#
# Optional:
#   PORTCULLIS_E2E_EXTRA_HEADERS='{"x-opencode-session":"e2e"}'  static upstream headers
#   PORTCULLIS_E2E_BIN=./target/release/portcullis                binary to test
#
# Deliberately runs with the ONNX detector DISABLED: the ML layer is covered by
# the golden test, and the deterministic dictionary + regex layers are enough to
# prove the proxy plumbing. That also keeps this fast and low-memory.

set -uo pipefail

URL=${PORTCULLIS_E2E_UPSTREAM_URL:?set PORTCULLIS_E2E_UPSTREAM_URL}
KEY=${PORTCULLIS_E2E_UPSTREAM_KEY:?set PORTCULLIS_E2E_UPSTREAM_KEY}
MODEL=${PORTCULLIS_E2E_MODEL:?set PORTCULLIS_E2E_MODEL}
BIN=${PORTCULLIS_E2E_BIN:-./target/release/portcullis}
EXTRA_HEADERS=${PORTCULLIS_E2E_EXTRA_HEADERS:-}
PORT=${PORTCULLIS_E2E_PORT:-18080}
BASE=http://127.0.0.1:$PORT
TOKEN=e2e-admin-token
STORE=$(mktemp -u /tmp/portcullis-e2e-XXXX.json)

pass=0; fail=0
ok()   { echo "  PASS  $1"; pass=$((pass+1)); }
bad()  { echo "  FAIL  $1"; fail=$((fail+1)); }
check(){ if [ "$2" = "1" ]; then ok "$1"; else bad "$1"; fi; }

cleanup() { [ -n "${PID:-}" ] && kill "$PID" 2>/dev/null; rm -f "$STORE"; }
trap cleanup EXIT

echo "== starting portcullis (upstream: $URL, model: $MODEL) =="
PORTCULLIS_UPSTREAM_URL="$URL" \
PORTCULLIS_UPSTREAM_KEY="$KEY" \
PORTCULLIS_UPSTREAM_HEADERS="$EXTRA_HEADERS" \
PORTCULLIS_FORWARD_HEADERS="x-opencode-session" \
PORTCULLIS_ADMIN_TOKEN="$TOKEN" \
PORTCULLIS_STORE="$STORE" \
PORTCULLIS_MODEL_DIR=/nonexistent \
PORTCULLIS_LOG=warn \
  "$BIN" serve --bind "127.0.0.1:$PORT" &
PID=$!

for _ in $(seq 1 40); do
  curl -sf "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.25
done

echo
echo "== 0. /healthz =="
HEALTH=$(curl -sS "$BASE/healthz")
echo "  $HEALTH"
echo "$HEALTH" | grep -q '"status":"ok"' && check "healthz responds ok" 1 || check "healthz responds ok" 0

echo
echo "== 1. teach via the admin surface, then confirm it is listed =="
curl -sS -X POST "$BASE/teach" -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"term":"Cartalian","label":"ORG"}' >/dev/null
TERMS=$(curl -sS "$BASE/terms" -H "Authorization: Bearer $TOKEN")
echo "  $TERMS"
echo "$TERMS" | grep -q 'Cartalian' && check "taught term is listed" 1 || check "taught term is listed" 0

echo
echo "== 2. non-streaming round trip =="
BODY='{"model":"'"$MODEL"'","max_tokens":80,"messages":[{"role":"user","content":"Repeat this sentence exactly and nothing else: Cartalian is based in Zurich."}]}'
RESP=$(curl -sS --max-time 120 "$BASE/v1/chat/completions" \
  -H 'content-type: application/json' -H "x-opencode-session: e2e" -d "$BODY")
echo "  $(echo "$RESP" | head -c 300)"
echo "$RESP" | grep -q '"choices"' && check "returned a completion" 1 || check "returned a completion" 0
echo "$RESP" | grep -q 'Cartalian' && check "real value rehydrated for the client" 1 || check "real value rehydrated for the client" 0
echo "$RESP" | grep -qE '<<[A-Z_]+_[0-9]+>>' && check "no placeholder leaked to the client" 0 || check "no placeholder leaked to the client" 1

echo
echo "== 3. streaming round trip (the path a mock cannot prove) =="
SBODY='{"model":"'"$MODEL"'","max_tokens":80,"stream":true,"messages":[{"role":"user","content":"Repeat this sentence exactly and nothing else: Cartalian is based in Zurich."}]}'
STREAM=$(curl -sS -N --max-time 120 "$BASE/v1/chat/completions" \
  -H 'content-type: application/json' -H "x-opencode-session: e2e" -d "$SBODY")
echo "  first 200 bytes: $(echo "$STREAM" | head -c 200)"
echo "$STREAM" | grep -q '^data: {' && check "emits SSE data frames" 1 || check "emits SSE data frames" 0
echo "$STREAM" | grep -q '\[DONE\]' && check "terminates with [DONE]" 1 || check "terminates with [DONE]" 0
echo "$STREAM" | grep -q 'Cartalian' && check "streamed value rehydrated" 1 || check "streamed value rehydrated" 0
echo "$STREAM" | grep -qE '<<[A-Z_]+_[0-9]+>>' && check "no placeholder leaked in the stream" 0 || check "no placeholder leaked in the stream" 1
# A half-placeholder is the failure mode the carry buffer exists to prevent.
echo "$STREAM" | grep -qE '<<[A-Z_]*$|^[A-Z_]*>>' && check "no half-placeholder split across frames" 0 || check "no half-placeholder split across frames" 1

echo
echo "== 4. the provider received a placeholder, not the real value =="
# Ask about a property of the text the model saw. The placeholder <<ORG_1>>
# contains angle brackets; the real value does not. The question itself avoids
# angle brackets, so it cannot confuse the answer.
# Note: this is a model-judgment probe. It is the only end-to-end check that the
# provider received a placeholder, so it is worth the fuzziness; the
# deterministic proof lives in the mock-upstream tests.
# max_tokens is generous because reasoning models spend most of their budget on
# reasoning_content before emitting any content.
PROBE='{"model":"'"$MODEL"'","max_tokens":600,"messages":[
 {"role":"user","content":"My employer, Cartalian, is based in Zurich."},
 {"role":"assistant","content":"Noted."},
 {"role":"user","content":"Look at my first message. Does the text contain any angle bracket characters? Answer with exactly YES or NO, nothing else."}]}'
PROBE_RESP=$(curl -sS --max-time 180 "$BASE/v1/chat/completions" \
  -H 'content-type: application/json' -H "x-opencode-session: e2e" -d "$PROBE")
ANSWER=$(echo "$PROBE_RESP" | python3 -c \
  'import json,sys
try:
    d=json.load(sys.stdin)
    m=d["choices"][0]["message"]
    print((m.get("content") or m.get("reasoning_content") or "").strip())
except Exception as e:
    print("")' 2>/dev/null)
echo "  model answered: '$ANSWER'"
UPPER=$(echo "$ANSWER" | tr '[:lower:]' '[:upper:]')
case "$UPPER" in
  *YES*) check "provider saw a placeholder (angle brackets present)" 1 ;;
  *)     check "provider saw a placeholder (angle brackets present)" 0 ;;
esac

echo
echo "== 5. upstream error shape surfaces sanely (not a hang, not a panic) =="
BAD=$(curl -sS --max-time 60 -o /dev/null -w '%{http_code}' \
  "$BASE/v1/chat/completions" -H 'content-type: application/json' \
  -H "x-opencode-session: e2e" \
  -d '{"model":"definitely-not-a-real-model-xyz","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}')
echo "  HTTP $BAD"
[ "$BAD" != "000" ] && check "upstream error returned a status (no hang)" 1 || check "upstream error returned a status (no hang)" 0

echo
echo "===================================="
echo "  $pass passed, $fail failed"
echo "===================================="
[ "$fail" -eq 0 ]
