#!/usr/bin/env bash
# 停止 CSSwitch 代理容器。
set -euo pipefail

cd "$(dirname "$0")/.."

echo "停止 CSSwitch 代理容器…"
docker compose -f docker/docker-compose.yml down
echo "已停止。"
