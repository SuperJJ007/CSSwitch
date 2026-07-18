#!/usr/bin/env bash
# S0 frontend 层：node --check 语法检查（无框架、无构建）。无 node → env-blocked。
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
if ! command -v node >/dev/null 2>&1; then
  echo "S0_LAYER frontend env-blocked (no node)"; exit 0
fi
fail=0
for f in desktop/src/main.js desktop/src/codex-auth-protocol.js desktop/src/runtime-status-state.js desktop/src/model-catalog-state.js test/codex_auth_ui.test.mjs test/runtime_status_state.test.mjs test/model_catalog_state.test.mjs desktop/src/skill-page.js desktop/src/skill-page-state.js test/skill_page_state.test.mjs; do
  if node --check "$f"; then echo "ok - node --check $f"; else echo "NOT ok - $f"; fail=1; fi
done
if node --test test/codex_auth_ui.test.mjs; then
  echo "ok - Codex auth UI structured error tests"
else
  echo "NOT ok - Codex auth UI structured error tests"
  fail=1
fi
if node --test test/skill_page_state.test.mjs; then
  echo "ok - node --test real Skill page state"
else
  echo "NOT ok - real Skill page state tests"; fail=1
fi
if node --test test/runtime_status_state.test.mjs; then
  echo "ok - node --test runtime status state"
else
  echo "NOT ok - runtime status state tests"; fail=1
fi
if node --test test/model_catalog_state.test.mjs; then
  echo "ok - node --test provider model catalog state"
else
  echo "NOT ok - provider model catalog state tests"; fail=1
fi
if [ "$fail" -eq 0 ]; then echo "S0_LAYER frontend pass"; exit 0; else echo "S0_LAYER frontend fail"; exit 1; fi
