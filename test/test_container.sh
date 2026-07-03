#!/usr/bin/env bash
# CSSwitch 容器冒烟测试。
# 受 RUN_CONTAINER_TESTS=1 环境变量控制，默认跳过。
# 依赖：docker 与 docker compose。
set -euo pipefail

if [ "${RUN_CONTAINER_TESTS:-0}" != "1" ]; then
  echo "  跳过容器测试（RUN_CONTAINER_TESTS=1 时启用）"
  exit 0
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# ── 依赖检查 ──
if ! command -v docker >/dev/null 2>&1; then
  echo "  SKIP: 无 docker 命令"
  exit 0
fi
if ! docker compose version >/dev/null 2>&1; then
  echo "  SKIP: 无 docker compose 插件"
  exit 0
fi

PASS=0; FAIL=0
pass() { echo "  ✓ $1"; PASS=$((PASS + 1)); }
fail() { echo "  ✗ $1"; FAIL=$((FAIL + 1)); }

# ── 清理函数 ──
cleanup() {
  echo "  清理容器…"
  docker compose -f docker/docker-compose.yml down 2>/dev/null || true
}
trap cleanup EXIT

echo "== container smoke test =="

# ── 1. 构建镜像 ──
echo "  [构建]"
if docker build -t csswitch-proxy:test -f docker/Dockerfile . >/dev/null 2>&1; then
  pass "镜像构建成功"
else
  fail "镜像构建失败"
  echo "  FAIL: 构建失败，跳过后续测试"
  exit 1
fi

# ── 2. 准备临时 .env ──
echo "  [配置]"
cp docker/.env.example docker/.env
# 写入 dummy key（不会真正调用上游，只测代理启动与健康检查）
cat >> docker/.env <<'EOF'
DEEPSEEK_API_KEY=sk-test-dummy-key
CSSWITCH_AUTH_TOKEN=test-secret-123
CSSWITCH_PROXY_PORT=18991
EOF
pass "临时 .env 就绪（dummy key）"

# ── 3. 启动容器 ──
echo "  [启动]"
if docker compose -f docker/docker-compose.yml up -d 2>/dev/null; then
  pass "容器启动成功"
else
  fail "容器启动失败"
  exit 1
fi

# ── 4. 等待健康检查 ──
echo "  [健康检查]"
HEALTH_OK=0
for i in $(seq 1 10); do
  if curl -s -m 3 http://127.0.0.1:18991/test-secret-123/health >/dev/null 2>&1; then
    HEALTH_OK=1
    break
  fi
  sleep 2
done
if [ "$HEALTH_OK" = "1" ]; then
  pass "健康检查通过（/health 200）"
else
  fail "健康检查超时"
  docker compose -f docker/docker-compose.yml logs --tail=20 csswitch-proxy 2>/dev/null || true
fi

# ── 5. 验证 /v1/models ──
if curl -s -m 5 http://127.0.0.1:18991/test-secret-123/v1/models | grep -q '"data"' 2>/dev/null; then
  pass "/v1/models 返回模型列表"
else
  fail "/v1/models 未返回预期数据"
fi

# ── 汇总 ──
echo "----"
echo "容器测试：通过 ${PASS}，失败 ${FAIL}"
[ "$FAIL" -eq 0 ] || exit 1
echo "ALL CONTAINER TESTS GREEN"
