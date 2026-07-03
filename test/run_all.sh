#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
echo "== python unittest =="
python3 -m unittest discover -s test -p 'test_*.py' -v
echo "== node --test =="
node --test test/test_make_virtual_oauth.mjs
echo "== bash scripts =="
bash test/test_scripts.sh
echo "== bash ops scripts (doctor/verify-proxy/self-test) =="
bash test/test_ops_scripts.sh
echo "== container smoke test (RUN_CONTAINER_TESTS=1) =="
bash test/test_container.sh
echo "ALL GREEN"
