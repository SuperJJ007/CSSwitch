#!/usr/bin/env bash
# S0 scripts layer: shell contracts and operational checks.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
fail=0
echo "== bash scripts =="
bash test/test_scripts.sh || fail=1
echo "== ops (doctor 契约 + verify-proxy 自门) =="
bash test/test_ops_scripts.sh || fail=1
if [ "$fail" -eq 0 ]; then echo "S0_LAYER scripts pass"; exit 0; else echo "S0_LAYER scripts fail"; exit 1; fi
