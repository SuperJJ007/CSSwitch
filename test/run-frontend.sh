#!/usr/bin/env bash
# S0 frontend 层：node --check 语法检查（无框架、无构建）。无 node → env-blocked。
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
if ! command -v node >/dev/null 2>&1; then
  echo "S0_LAYER frontend env-blocked (no node)"; exit 0
fi
fail=0
for f in desktop/src/main.js desktop/src/codex-auth-protocol.js test/codex_auth_ui.test.mjs; do
  if node --check "$f"; then echo "ok - node --check $f"; else echo "NOT ok - $f"; fail=1; fi
done
if node --test test/codex_auth_ui.test.mjs; then
  echo "ok - Codex auth UI structured error tests"
else
  echo "NOT ok - Codex auth UI structured error tests"
  fail=1
fi
if [ "$fail" -eq 0 ]; then echo "S0_LAYER frontend pass"; exit 0; else echo "S0_LAYER frontend fail"; exit 1; fi
