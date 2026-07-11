#!/usr/bin/env bash
# S0 离线纯单元层：无 loopback / 无网络 / 无上游。
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
if ! command -v python3 >/dev/null 2>&1; then
  echo "S0_LAYER offline env-blocked (no python3)"; exit 0
fi
if python3 -m unittest test.test_capability test.test_capability_catalog test.test_process_ownership_policy -v; then
  echo "S0_LAYER offline pass"; exit 0
else
  echo "S0_LAYER offline fail"; exit 1
fi
