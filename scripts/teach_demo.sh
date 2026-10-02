#!/usr/bin/env bash
# Demonstrate exactly how teaching works — and where it breaks.
#
# Referenced by docs/TEACHING.md. Uses a throwaway store, so it is safe to run.
#
#   cargo build --release --features onnx
#   bash scripts/teach_demo.sh
#
# Set PORTCULLIS_MODEL_DIR=/nonexistent to see the deterministic layers alone.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

BIN=${PORTCULLIS_BIN:-./target/release/portcullis}
S=$(mktemp -u "${TMPDIR:-/tmp}/portcullis-demo-XXXX.json")
trap 'rm -f "$S"' EXIT

show() {
  echo "── $1"
  "$BIN" --store "$S" scan "$2"
  echo
}

TEXT1="Hi, I'm Daniel Pratt from Cartalian. Email daniel.pratt@northwind-logistics.com, card 4111 1111 1111 1111."

echo "############ 1. NOTHING TAUGHT — what does it catch on its own? ############"
show "empty store" "$TEXT1"

echo "############ 2. AFTER TEACHING ONE TERM ############"
"$BIN" --store "$S" teach Cartalian --label ORG >/dev/null 2>&1
show "taught: Cartalian (ORG)" "$TEXT1"

echo "############ 3. WHICH VARIANTS DOES IT CATCH? ############"
show "surface forms" "Cartalian cartalian CARTALIAN Cartalian's Cartalian Inc. Cartalia CTL"

echo "############ 4. THE OVER-MATCH TRAP (no word boundaries) ############"
"$BIN" --store "$S" teach Ann --label PERSON >/dev/null 2>&1
show "taught: Ann (PERSON)" "Ann is the contact. See the Announcement and the Annual report."

echo "############ 5. THE STORE — this is the whole of what it 'learned' ############"
cat "$S"