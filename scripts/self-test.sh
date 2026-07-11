#!/usr/bin/env bash
# CSSwitch self-test：跑离线回归套件（test/run_all.sh）。
#   隔离环境，只运行本地代理和脚本单元测试，不碰真实用户目录、不联网上游。
#   安装后自检、或改动后回归都可直接跑这一条。
set -u
PROJ="$(cd "$(dirname "$0")/.." && pwd)"
echo "CSSwitch self-test → 离线回归套件（隔离，不碰 Science、不联网）"
exec bash "$PROJ/test/run_all.sh"
