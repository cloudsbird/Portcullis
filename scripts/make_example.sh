#!/usr/bin/env bash
# Produce the real before/after output quoted in docs/EXAMPLE.md.
# Requires a release build (and ./model present for the ONNX layer).
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.." || exit 1

S="${TMPDIR:-/tmp}/portcullis-demo-store.json"
rm -f "$S"

PC="./target/release/portcullis --store $S"
$PC teach Cartalian --label ORG >/dev/null
$PC teach "Daniel Pratt" --label PERSON >/dev/null
$PC teach "Northwind Logistics" --label ORG >/dev/null

echo "### 1. SYSTEM PROMPT"
$PC scan "You are the assistant for Northwind Logistics. The account manager for Cartalian is Daniel Pratt."
echo
echo "### 2. USER PROMPT"
$PC scan "Draft a follow-up to Daniel Pratt <daniel.pratt@northwind-logistics.com> about the Cartalian renewal. My direct line is +1 415 555 0132."
echo
echo "### 3. NOT-YET-TAUGHT TERM — the model still catches it, but the label is a guess"
$PC scan "Also mention that Project Loki kicks off next month."
echo
echo "### 4. teach Project Loki"
$PC teach "Project Loki" --label ORG
echo
echo "### 5. THE SAME TEXT AGAIN — now redacted deterministically"
$PC scan "Also mention that Project Loki kicks off next month."

rm -f "$S"
