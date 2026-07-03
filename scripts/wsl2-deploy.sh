#!/usr/bin/env bash
# 在 WSL2 / Linux 上启动 CSSwitch 代理容器。
#   检查依赖与 .env，调用 docker compose up -d，输出连接地址。
set -euo pipefail

cd "$(dirname "$0")/.."

# ── 依赖检查 ──
if ! command -v docker >/dev/null 2>&1; then
  echo "✗ 未找到 docker。请先安装 Docker Engine 或 Docker Desktop。"
  echo "  参考：https://docs.docker.com/engine/install/"
  exit 1
fi

if ! docker compose version >/dev/null 2>&1; then
  echo "✗ 未找到 docker compose 插件。请安装 Docker Compose v2 插件。"
  exit 1
fi

# ── .env 检查 ──
ENV_FILE="docker/.env"
if [ ! -f "$ENV_FILE" ]; then
  echo "✗ 未找到 $ENV_FILE"
  echo "  从模板复制：cp docker/.env.example docker/.env"
  echo "  然后编辑 docker/.env，填入你的 API key。"
  exit 1
fi

# 提示检查 key（只检查变量名是否存在，不读值）
set +u
source <(grep -E '^(DEEPSEEK_API_KEY|DASHSCOPE_API_KEY)=' "$ENV_FILE" 2>/dev/null)
PROVIDER="${CSSWITCH_PROVIDER:-deepseek}"
if [ "$PROVIDER" = "deepseek" ] && [ -z "${DEEPSEEK_API_KEY:-}" ]; then
  echo "⚠   CSSWITCH_PROVIDER=deepseek，但 DEEPSEEK_API_KEY 在 .env 中未设置"
  echo "   继续启动，但代理会因缺 key 退出。"
elif [ "$PROVIDER" = "qwen" ] && [ -z "${DASHSCOPE_API_KEY:-}" ]; then
  echo "⚠   CSSWITCH_PROVIDER=qwen，但 DASHSCOPE_API_KEY 在 .env 中未设置"
  echo "   继续启动，但代理会因缺 key 退出。"
fi
set -u

# ── 启动 ──
echo "启动 CSSwitch 代理容器…"
docker compose -f docker/docker-compose.yml up -d
echo

# ── 输出连接地址 ──
PORT="${CSSWITCH_PROXY_PORT:-18991}"
SECRET="${CSSWITCH_AUTH_TOKEN:-}"

# 探测 WSL2 IP（WSL2 内运行）
WSL2_IP=""
if [ -f /proc/version ] && grep -qi microsoft /proc/version; then
  WSL2_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
fi

echo "=== 代理已启动 ==="
echo "  容器内健康检查：curl http://127.0.0.1:$PORT/${SECRET:+$SECRET/}health"
if [ -n "$WSL2_IP" ]; then
  echo "  WSL2 IP：$WSL2_IP"
  echo "  Windows 客户端 ANTHROPIC_BASE_URL：http://$WSL2_IP:$PORT/${SECRET:+$SECRET/}"
fi
echo ""
echo "如需查看日志：bash scripts/wsl2-logs.sh"
echo "如需停止服务：bash scripts/wsl2-stop.sh"
