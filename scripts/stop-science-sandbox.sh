#!/bin/zsh
# 停止隔离沙箱 Science（只停沙箱 data-dir 的守护进程，绝不影响真实实例 8765）。
set -euo pipefail
PROJ="${0:A:h:h}"
SANDBOX_HOME="${SANDBOX_HOME:-$PROJ/.sandbox/home}"
DATA_DIR="$SANDBOX_HOME/.claude-science"
REAL_DIR="$HOME/.claude-science"
APP_BIN="/Applications/Claude Science.app/Contents/Resources/bin/claude-science"
BIN="${SCIENCE_BIN:-}"

reject_explicit_science_symlink() {
  local probe="$1"
  [[ "$probe" == /* ]] || { echo "拒绝：显式 SCIENCE_BIN 必须是绝对路径: $probe"; return 1; }
  while [[ "$probe" != "/" ]]; do
    [[ -L "$probe" ]] && { echo "拒绝：显式 SCIENCE_BIN 路径含符号链接: $probe"; return 1; }
    probe="${probe:h}"
  done
}
if [[ -n "${SCIENCE_BIN:-}" ]]; then
  reject_explicit_science_symlink "$BIN" || exit 1
elif [[ -x "$APP_BIN" ]]; then
  BIN="$APP_BIN"
else
  echo "找不到已安装的 Claude Science App；停止操作必须由 CSSwitch 传入本次启动使用的 SCIENCE_BIN" >&2
  exit 1
fi

_dd="${DATA_DIR:A}"; _rd="${REAL_DIR:A}"
if [[ "$_dd" == "$_rd" ]]; then echo "拒绝：data-dir 的真实路径指向真实目录"; exit 1; fi

if [[ ! -d "$DATA_DIR" ]]; then echo "沙箱不存在，无需停止。"; exit 0; fi
if [[ ! -x "$BIN" ]]; then echo "找不到 Science 二进制: $BIN" >&2; exit 1; fi

if HOME="$SANDBOX_HOME" "$BIN" stop --data-dir "$DATA_DIR" 2>&1 | tail -2; then
  echo "沙箱已停。真实实例 8765 未受影响。"
else
  rc=${pipestatus[1]:-$?}
  echo "停止失败（退出码 $rc）。真实实例 8765 未受影响。" >&2
  exit "$rc"
fi
