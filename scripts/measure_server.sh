#!/usr/bin/env bash
# Measure the proxy server's resident memory at steady state, for each config.
# Used to produce the numbers in docs/RESOURCES.md.
set -u
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.." || exit 1

run_case () {
  local label="$1" modeldir="$2" port="$3" wait_s="$4"
  PORTCULLIS_MODEL_DIR="$modeldir" \
  PORTCULLIS_UPSTREAM_URL="http://127.0.0.1:9/v1/chat/completions" \
  PORTCULLIS_UPSTREAM_KEY="x" \
  PORTCULLIS_ADMIN_TOKEN="t" \
  PORTCULLIS_STORE="/tmp/pc-srv-$$.json" \
    ./target/release/portcullis serve --bind "127.0.0.1:$port" >/dev/null 2>&1 &
  local pid=$!
  sleep "$wait_s"
  echo "--- $label ---"
  if [ -r "/proc/$pid/status" ]; then
    grep -E 'VmRSS|VmHWM' "/proc/$pid/status"
  else
    echo "process not running"
  fi
  kill "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  rm -f "/tmp/pc-srv-$$.json"
}

run_case "deterministic only (no model)" "/nonexistent" 8099 3
run_case "with ONNX detector (model loaded)" "./model" 8098 12
