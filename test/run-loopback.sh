#!/usr/bin/env bash
# S0 loopback 层：需 127.0.0.1 bind/connect。禁则整层 env-blocked（非 fail）。
# 已知低频 mock-timing race（跨测试类 /health 就绪等待不一致）：以有界重试在本层吸收，
# 绝不吞掉真失败：重试用尽仍失败则明确 fail，重试过程本身也全程可见（非静默）。
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
if ! command -v python3 >/dev/null 2>&1; then
  echo "S0_LAYER loopback env-blocked (no python3)"; exit 0
fi
if [ "$(python3 test/_capability.py)" != "1" ]; then
  echo "本环境禁止 loopback bind/connect，跳过 loopback 层。"
  echo "S0_LAYER loopback env-blocked (loopback not permitted)"; exit 0
fi

# The normal gate must exercise a binary built from the current gateway source.
# The injected command exists only for the runner's deterministic retry tests.
if [ -z "${CSSWITCH_LOOPBACK_TEST_CMD:-}" ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    [ -x "$HOME/.cargo/bin/cargo" ] && export PATH="$HOME/.cargo/bin:$PATH"
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    echo "S0_LAYER loopback env-blocked (no cargo for current Rust gateway build)"; exit 0
  fi
  if ! cargo build --manifest-path desktop/gateway/Cargo.toml; then
    echo "S0_LAYER loopback fail (current Rust gateway build failed)"; exit 1
  fi
  export CSSWITCH_GATEWAY_BIN="$ROOT/desktop/gateway/target/debug/csswitch-gateway"
  if [ ! -x "$CSSWITCH_GATEWAY_BIN" ]; then
    echo "S0_LAYER loopback fail (current Rust gateway binary missing)"; exit 1
  fi
fi

# CSSWITCH_LOOPBACK_TEST_CMD 仅测试用：注入确定性 pass/fail 桩以验证重试逻辑，不用于正常运行。
run_loopback_once() {
  if [ -n "${CSSWITCH_LOOPBACK_TEST_CMD:-}" ]; then
    eval "$CSSWITCH_LOOPBACK_TEST_CMD"
  else
    python3 -m unittest test.test_gateway_rust test.test_provider_mock_scenarios test.test_installed_provider_matrix -v
  fi
}

MAX_ATTEMPTS=3
attempt=1
while [ "$attempt" -le "$MAX_ATTEMPTS" ]; do
  if run_loopback_once; then
    if [ "$attempt" -gt 1 ]; then
      echo "loopback 第 $attempt 次尝试通过（此前 $((attempt - 1)) 次疑似 mock-timing race 失败）。"
    fi
    echo "S0_LAYER loopback pass"; exit 0
  fi
  if [ "$attempt" -lt "$MAX_ATTEMPTS" ]; then
    echo "loopback 尝试 $attempt/$MAX_ATTEMPTS 失败, 重试(已知低频 mock-timing race)"
  fi
  attempt=$((attempt + 1))
done
echo "S0_LAYER loopback fail"; exit 1
