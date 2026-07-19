#!/bin/bash
# 停止隔离沙箱 Science（只停沙箱 data-dir 的守护进程，绝不影响真实实例 8765）。
set -euo pipefail
umask 077
PROJ="$(cd "$(dirname "$0")/.." && pwd -P)"
SANDBOX_HOME="${SANDBOX_HOME:-$PROJ/.sandbox/home}"
DATA_DIR="$SANDBOX_HOME/.claude-science"
REAL_DATA_DIR="$HOME/.claude-science"
case "$(uname -s)" in
  Darwin) PLATFORM="macos"; APP_BIN="/Applications/Claude Science.app/Contents/Resources/bin/claude-science" ;;
  Linux) PLATFORM="linux"; APP_BIN="$HOME/.local/bin/claude-science" ;;
  *) echo "拒绝：当前平台不在 CSSwitch Science 支持范围内"; exit 1 ;;
esac
BIN="${SCIENCE_BIN:-}"

is_safe_science_bin() {
  local probe="$1"
  case "$probe" in /*) ;; *) return 1;; esac
  while [[ "$probe" != "/" ]]; do
    [[ -L "$probe" ]] && return 1
    probe="$(dirname "$probe")"
  done
  [[ -f "$1" && -x "$1" ]]
}
path_contains_symlink() {
  local probe="$1"
  case "$probe" in /*) ;; *) return 0;; esac
  while [[ "$probe" != "/" ]]; do
    [[ -L "$probe" ]] && return 0
    probe="$(dirname "$probe")"
  done
  return 1
}
if [[ -n "${SCIENCE_BIN:-}" ]] && ! is_safe_science_bin "$BIN"; then
  echo "拒绝：显式 SCIENCE_BIN 路径含符号链接或不是绝对可执行文件"
  exit 1
fi

canonical_candidate() {
  local probe="$1"
  local suffix=""
  local leaf
  while [[ ! -d "$probe" ]]; do
    leaf="$(basename "$probe")"
    suffix="${leaf}${suffix:+/$suffix}"
    probe="$(dirname "$probe")"
  done
  probe="$(cd "$probe" && pwd -P)"
  printf '%s' "$probe${suffix:+/$suffix}"
}

_dd="$(canonical_candidate "$DATA_DIR")"
_real_home="$(cd "$HOME" && pwd -P)"
_rd="$_real_home/.claude-science"
if [[ "$_dd" == "$_rd" ]]; then echo "拒绝：data-dir 的真实路径指向真实目录"; exit 1; fi
if path_contains_symlink "$DATA_DIR"; then
  echo "拒绝：Science data-dir 路径包含符号链接"
  exit 1
fi

if [[ ! -d "$DATA_DIR" ]]; then echo "沙箱不存在，无需停止。"; exit 0; fi

_SCIENCE_ENV=("HOME=$SANDBOX_HOME")
if [[ "$PLATFORM" == "linux" ]]; then
  XDG_CONFIG_HOME="$SANDBOX_HOME/.config"
  XDG_DATA_HOME="$SANDBOX_HOME/.local/share"
  XDG_CACHE_HOME="$SANDBOX_HOME/.cache"
  XDG_STATE_HOME="$SANDBOX_HOME/.local/state"
  XDG_RUNTIME_DIR="$SANDBOX_HOME/.xdg-runtime"
  SCIENCE_TMPDIR="$SANDBOX_HOME/.tmp"
  for private_dir in "$SANDBOX_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR" "$SCIENCE_TMPDIR"; do
    if path_contains_symlink "$private_dir"; then
      echo "拒绝：Science 隔离环境目录包含符号链接" >&2
      exit 1
    fi
    mkdir -p "$private_dir"
    chmod 700 "$private_dir"
  done
  _SCIENCE_ENV+=(
    "PATH=/usr/local/bin:/usr/bin:/bin"
    "XDG_CONFIG_HOME=$XDG_CONFIG_HOME"
    "XDG_DATA_HOME=$XDG_DATA_HOME"
    "XDG_CACHE_HOME=$XDG_CACHE_HOME"
    "XDG_STATE_HOME=$XDG_STATE_HOME"
    "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
    "TMPDIR=$SCIENCE_TMPDIR"
  )
  [[ -n "${LANG:-}" ]] && _SCIENCE_ENV+=("LANG=$LANG")
  [[ -n "${LC_ALL:-}" ]] && _SCIENCE_ENV+=("LC_ALL=$LC_ALL")
  [[ -n "${LC_CTYPE:-}" ]] && _SCIENCE_ENV+=("LC_CTYPE=$LC_CTYPE")
fi

science_run() {
  if [[ "$PLATFORM" == "linux" ]]; then
    /usr/bin/env -i "${_SCIENCE_ENV[@]}" "$@"
  else
    /usr/bin/env "${_SCIENCE_ENV[@]}" "$@"
  fi
}

# Match launch identity. CSSwitch passes the exact runtime recorded at launch;
# without it, manual stop may use only the installed App and never an implicit
# data-dir fallback.
if [[ -z "$BIN" ]]; then
  if is_safe_science_bin "$APP_BIN" && science_run "$APP_BIN" --version >/dev/null 2>&1; then
    BIN="$APP_BIN"
  fi
fi
if ! is_safe_science_bin "$BIN"; then
  echo "找不到可用于停止沙箱的已验证 Science binary" >&2
  exit 1
fi

if path_contains_symlink "$DATA_DIR"; then
  echo "拒绝：Science data-dir 路径在停止前发生符号链接变化" >&2
  exit 1
fi
if science_run "$BIN" stop --data-dir "$DATA_DIR" 2>&1 | tail -2; then
  echo "沙箱已停。真实实例 8765 未受影响。"
else
  rc=${PIPESTATUS[0]:-$?}
  echo "停止失败（退出码 ${rc}）。真实实例 8765 未受影响。" >&2
  exit "$rc"
fi
