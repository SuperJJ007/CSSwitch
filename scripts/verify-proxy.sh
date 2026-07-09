#!/usr/bin/env bash
# 校验一个【正在运行】的 CSSwitch 代理：GET /health 与 GET /v1/models。
#   - 只读、只打本地回环代理；不启动 Science、不动真实目录。
#   - /health 与 /v1/models 由代理本地作答，不触发任何上游调用，零花费。
# 用法：verify-proxy.sh [--port 18991] [--secret <path-secret>] [--host 127.0.0.1]
set -u

HOST="127.0.0.1"
PORT="${CSSWITCH_PROXY_PORT:-18991}"
SECRET=""
while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="$2"; shift 2;;
    --secret) SECRET="$2"; shift 2;;
    --host) HOST="$2"; shift 2;;
    *) echo "未知参数：$1"; exit 2;;
  esac
done

[[ "$PORT" =~ ^[0-9]+$ ]] || { echo "✗ 端口非法整数：$PORT"; exit 2; }
if [ "$((10#$PORT))" -eq 8765 ]; then
  echo "✗ 端口 8765 是真实 Science 实例保留端口，verify-proxy 拒绝访问。"
  exit 2
fi
PREFIX=""
[ -n "$SECRET" ] && PREFIX="/$SECRET"
BASE="http://$HOST:$PORT$PREFIX"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

echo "校验运行中的代理：http://$HOST:$PORT$([ -n "$SECRET" ] && echo /••••)"

code="$(curl -s -m 5 -o "$TMP" -w '%{http_code}' "$BASE/health" 2>/dev/null)" || code="000"
if [ "$code" = "200" ] && grep -q '"status"' "$TMP" && grep -q '"ok"' "$TMP"; then
  prov="$(grep -o '"provider"[^,}]*' "$TMP" | head -1)"
  echo "  ✓ /health 200（${prov}）"
else
  echo "  ✗ /health 未通过（HTTP ${code}）。代理未运行、端口不对，或 secret 错误。"
  exit 1
fi

code="$(curl -s -m 5 -o "$TMP" -w '%{http_code}' "$BASE/v1/models" 2>/dev/null)" || code="000"
if [ "$code" = "200" ] && grep -q '"data"' "$TMP"; then
  n="$(grep -o '"id"' "$TMP" | wc -l | tr -d ' ')"
  echo "  ✓ /v1/models 200（广告 ${n} 个模型）"
else
  echo "  ✗ /v1/models 未通过（HTTP ${code}）"
  exit 1
fi

echo "代理校验通过：http://$HOST:$PORT"
exit 0
