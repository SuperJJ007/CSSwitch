#!/usr/bin/env bash
# 查看 CSSwitch 代理容器日志。
set -euo pipefail

cd "$(dirname "$0")/.."

docker compose -f docker/docker-compose.yml logs -f csswitch-proxy
