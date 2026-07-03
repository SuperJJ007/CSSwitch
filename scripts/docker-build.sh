#!/usr/bin/env bash
# 构建 CSSwitch 代理容器镜像。
set -euo pipefail

cd "$(dirname "$0")/.."

echo "构建 CSSwitch 代理容器镜像…"
docker build -t csswitch-proxy:latest -f docker/Dockerfile .
echo "构建完成：csswitch-proxy:latest"
docker images csswitch-proxy:latest
