#!/usr/bin/env bash
# S0 rust 层：cargo fmt+clippy+test。无 cargo → env-blocked。无 loopback → 跳过端口 bind 测试并把本层标为 env-blocked。
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT/desktop/src-tauri"

if ! command -v cargo >/dev/null 2>&1; then
  [ -x "$HOME/.cargo/bin/cargo" ] && export PATH="$HOME/.cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "S0_LAYER rust env-blocked (no cargo)"; exit 0
fi

fail=0
blocked=0
cargo fmt --check || fail=1
cargo clippy --all-targets -- -D warnings || fail=1
# 端口 bind 测试名单（Step 1 定位于 scratch.rs；无 loopback 时 skip 并标 env-blocked）
PORT_TESTS="pick_scratch_port_returns_usable_nonreserved_port two_picks_are_bindable loopback_port_occupancy_probe_detects_listener_without_http"
if [ "$(python3 "$ROOT/test/_capability.py")" = "1" ]; then
  cargo test || fail=1
else
  blocked=1
  echo "loopback 禁 → 跳过端口 bind 测试，本 rust 层标记 env-blocked：$PORT_TESTS"
  skip_args=""; for t in $PORT_TESTS; do skip_args="$skip_args --skip $t"; done
  cargo test -- $skip_args || fail=1
fi

cd "$ROOT/desktop/gateway"
cargo fmt --check || fail=1
cargo clippy --all-targets -- -D warnings || fail=1
cargo test || fail=1

if [ "$fail" -ne 0 ]; then echo "S0_LAYER rust fail"; exit 1; fi
if [ "$blocked" -ne 0 ]; then echo "S0_LAYER rust env-blocked (loopback bind tests skipped)"; exit 0; fi
echo "S0_LAYER rust pass"; exit 0
