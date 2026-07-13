#!/bin/zsh
# 停止隔离沙箱 Science（只停沙箱 data-dir 的守护进程，绝不影响真实实例 8765）。
set -euo pipefail
PROJ="${0:A:h:h}"
SANDBOX_HOME="${SANDBOX_HOME:-$PROJ/.sandbox/home}"
DATA_DIR="$SANDBOX_HOME/.claude-science"
REAL_DATA_DIR="$HOME/.claude-science"
APP_BIN="/Applications/Claude Science.app/Contents/Resources/bin/claude-science"
BIN="${SCIENCE_BIN:-}"

is_safe_science_bin() {
  local probe="$1"
  [[ "$probe" == /* ]] || return 1
  while [[ "$probe" != "/" ]]; do
    [[ -L "$probe" ]] && return 1
    probe="${probe:h}"
  done
  [[ -f "$1" && -x "$1" ]]
}
if [[ -n "${SCIENCE_BIN:-}" ]] && ! is_safe_science_bin "$BIN"; then
  echo "拒绝：显式 SCIENCE_BIN 路径含符号链接或不是绝对可执行文件"
  exit 1
fi

_dd="${DATA_DIR:A}"; _rd="${REAL_DATA_DIR:A}"
if [[ "$_dd" == "$_rd" ]]; then echo "拒绝：data-dir 的真实路径指向真实目录"; exit 1; fi

if [[ ! -d "$DATA_DIR" ]]; then echo "沙箱不存在，无需停止。"; exit 0; fi

# Match launch selection: official local install first, retained sandbox binary as fallback.
if [[ -z "$BIN" ]]; then
  if is_safe_science_bin "$APP_BIN" && HOME="$SANDBOX_HOME" "$APP_BIN" --version >/dev/null 2>&1; then
    BIN="$APP_BIN"
  elif is_safe_science_bin "$DATA_DIR/bin/claude-science" && HOME="$SANDBOX_HOME" "$DATA_DIR/bin/claude-science" --version >/dev/null 2>&1; then
    BIN="$DATA_DIR/bin/claude-science"
  fi
fi
if [[ ! -x "$BIN" ]]; then echo "找不到可用于停止沙箱的 Science 二进制" >&2; exit 1; fi

if HOME="$SANDBOX_HOME" "$BIN" stop --data-dir "$DATA_DIR" 2>&1 | tail -2; then
  echo "沙箱已停。真实实例 8765 未受影响。"
else
  rc=${pipestatus[1]:-$?}
  echo "停止失败（退出码 $rc）。真实实例 8765 未受影响。" >&2
  exit "$rc"
fi
