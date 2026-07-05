#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
echo "== python unittest =="
python3 -m unittest discover -s test -p 'test_*.py' -v
echo "== node --test =="
node --test test/test_make_virtual_oauth.mjs
node --test test/test_desktop_ui_contract.mjs
echo "== bash scripts =="
bash test/test_scripts.sh
echo "== bash ops scripts (doctor/verify-proxy/self-test) =="
bash test/test_ops_scripts.sh
echo "ALL GREEN"
