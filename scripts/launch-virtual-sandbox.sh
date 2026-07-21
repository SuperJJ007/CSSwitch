#!/bin/zsh
# 启动 CSSwitch 管理的隔离运行环境。
# Safety boundaries:
#   - 独立 HOME + 独立 data-dir + 独立端口，绝不修改/删除真实 ~/.claude-science，绝不用端口 8765
#   - data-dir 只承载持久化状态；不从真实 Science HOME 读取或复制 runtime 或用户数据
#   - 系统 SSH 仅在显式授权时投影安全 Host alias；连接仍由 OpenSSH 读取真实配置
#   - 只使用应用在隔离目录中生成的本地状态，与真实账号无关
#   - 使用独立的本地钥匙串
#
# 用法:
#   代理由 CSSwitch 桌面端启动并管理；本脚本只负责虚拟 Science 沙箱。
#   再起沙箱: scripts/launch-virtual-sandbox.sh [--port 8990] [--proxy-url http://127.0.0.1:18991]
#   CSSwitch 桌面端通过 CSSWITCH_PROXY_URL 环境变量传递含 secret 的 URL，避免进入 argv。
set -euo pipefail
umask 077

PROJ="${0:A:h:h}"
SANDBOX_HOME="${SANDBOX_HOME:-$PROJ/.sandbox/home}"
DATA_DIR="$SANDBOX_HOME/.claude-science"   # = auth_dir（Science 按 HOME 推导）
REAL_HOME="$HOME"
REAL_DATA_DIR="$REAL_HOME/.claude-science"
APP_BIN="/Applications/Claude Science.app/Contents/Resources/bin/claude-science"
BIN="${SCIENCE_BIN:-}"
REUSE_SYSTEM_SSH="${CSSWITCH_REUSE_SYSTEM_SSH:-0}"
SYSTEM_SSH_CONFIG="$REAL_HOME/.ssh/config"
SSH_BRIDGE_DIR="$PROJ/scripts/ssh-bridge"
SSH_BRIDGE_BIN="$SSH_BRIDGE_DIR/ssh"
SANDBOX_SSH_DIR="$SANDBOX_HOME/.ssh"
SANDBOX_SSH_CONFIG="$SANDBOX_SSH_DIR/config"
SSH_STUB_V1_MARKER="# CSSwitch managed system SSH config bridge v1"
SSH_STUB_V2_MARKER="# CSSwitch managed system SSH config bridge v2"
SSH_STUB_MAX_ALIASES=512
SSH_STUB_MAX_BYTES=65536
PORT=8990
PROXY_URL="${CSSWITCH_PROXY_URL:-http://127.0.0.1:18991}"
EMAIL="virtual@localhost.invalid"
DRY_RUN=0
SKIP_FORGE=0

is_safe_science_bin() {
  local probe="$1"
  [[ "$probe" == /* ]] || return 1
  while [[ "$probe" != "/" ]]; do
    [[ -L "$probe" ]] && return 1
    probe="${probe:h}"
  done
  [[ -f "$1" && -x "$1" ]]
}

path_contains_symlink() {
  local probe="$1"
  [[ "$probe" == /* ]] || return 0
  while [[ "$probe" != "/" ]]; do
    [[ -L "$probe" ]] && return 0
    probe="${probe:h}"
  done
  return 1
}

escaped_system_ssh_include() {
  local escaped
  escaped="$(printf '%s' "$SYSTEM_SSH_CONFIG" | /usr/bin/sed 's/\\/\\\\/g; s/"/\\"/g')"
  printf 'Include "%s"\n' "$escaped"
}

is_managed_ssh_stub_v1() {
  local target="$1" expected_include
  expected_include="$(escaped_system_ssh_include)"
  printf '%s\n%s\n' "$SSH_STUB_V1_MARKER" "$expected_include" | /usr/bin/cmp -s - "$target"
}

is_managed_ssh_stub_v2() {
  local target="$1" expected_include last_byte size
  size="$(/usr/bin/stat -f '%z' "$target" 2>/dev/null)" || return 1
  [[ "$size" =~ ^[0-9]+$ ]] || return 1
  (( 10#${size} <= SSH_STUB_MAX_BYTES )) || return 1
  last_byte="$(/usr/bin/tail -c 1 "$target" 2>/dev/null | /usr/bin/od -An -tuC | /usr/bin/tr -d '[:space:]')"
  [[ "$last_byte" == "10" ]] || return 1
  expected_include="$(escaped_system_ssh_include)"
  LC_ALL=C /usr/bin/awk \
    -v marker="$SSH_STUB_V2_MARKER" \
    -v include="$expected_include" \
    -v max_aliases="$SSH_STUB_MAX_ALIASES" '
      NR == 1 { if ($0 != marker) exit 1; next }
      NR == 2 { if ($0 != include) exit 1; next }
      {
        if ($0 !~ /^Host [-A-Za-z0-9._:@%+]+$/) exit 1
        alias = substr($0, 6)
        if (seen[alias]++) exit 1
        count++
        if (count > max_aliases) exit 1
      }
      END { if (NR < 2) exit 1 }
    ' "$target" >/dev/null 2>&1
}

is_managed_ssh_stub() {
  local target="$1"
  [[ -f "$target" && ! -L "$target" ]] || return 1
  [[ "$(/usr/bin/stat -f '%u' "$target" 2>/dev/null)" == "$(/usr/bin/id -u)" ]] || return 1
  is_managed_ssh_stub_v1 "$target" || is_managed_ssh_stub_v2 "$target"
}

append_projected_ssh_aliases() {
  local target="$1"
  LC_ALL=C /usr/bin/awk -v max_aliases="$SSH_STUB_MAX_ALIASES" '
    function emit(candidate) {
      if (candidate !~ /^[-A-Za-z0-9._:@%+]+$/) return
      if (seen[candidate]) return
      seen[candidate] = 1
      count++
      if (count > max_aliases) {
        overflow = 1
        exit 42
      }
      print "Host " candidate
    }
    {
      line = $0
      sub(/^[[:space:]]+/, "", line)
      if (line == "" || substr(line, 1, 1) == "#") next
      fields = split(line, token, /[[:space:]]+/)
      keyword = tolower(token[1])
      start = 0
      if (keyword == "host") {
        start = 2
      } else if (substr(keyword, 1, 5) == "host=") {
        emit(substr(token[1], 6))
        start = 2
      } else {
        next
      }
      for (i = start; i <= fields; i++) {
        if (substr(token[i], 1, 1) == "#") break
        emit(token[i])
      }
    }
    END { if (overflow) exit 42 }
  ' "$SYSTEM_SSH_CONFIG" >> "$target"
}

prepare_sandbox_ssh_config() {
  if path_contains_symlink "$SANDBOX_SSH_DIR"; then
    echo "拒绝：隔离 SSH 配置目录包含符号链接"
    return 1
  fi
  if [[ -e "$SANDBOX_SSH_DIR" && ! -d "$SANDBOX_SSH_DIR" ]]; then
    echo "拒绝：隔离 SSH 配置目录不是普通目录"
    return 1
  fi
  /bin/mkdir -p -m 700 "$SANDBOX_SSH_DIR" 2>/dev/null || {
    echo "拒绝：无法创建隔离 SSH 配置目录"
    return 1
  }
  /bin/chmod 700 "$SANDBOX_SSH_DIR" 2>/dev/null || {
    echo "拒绝：无法收紧隔离 SSH 配置目录权限"
    return 1
  }
  if [[ -e "$SANDBOX_SSH_CONFIG" || -L "$SANDBOX_SSH_CONFIG" ]]; then
    if ! is_managed_ssh_stub "$SANDBOX_SSH_CONFIG"; then
      echo "拒绝：隔离 SSH config 不是 CSSwitch 管理的安全入口"
      return 1
    fi
  fi
  local expected_include size tmp
  expected_include="$(escaped_system_ssh_include)"
  tmp="$(/usr/bin/mktemp "$SANDBOX_SSH_DIR/.csswitch-config.XXXXXX" 2>/dev/null)" || {
    echo "拒绝：无法创建隔离 SSH 临时配置"
    return 1
  }
  /bin/chmod 600 "$tmp" 2>/dev/null || {
    /bin/rm -f "$tmp" 2>/dev/null || true
    echo "拒绝：无法收紧隔离 SSH 临时配置权限"
    return 1
  }
  printf '%s\n%s\n' "$SSH_STUB_V2_MARKER" "$expected_include" > "$tmp" 2>/dev/null || {
    /bin/rm -f "$tmp" 2>/dev/null || true
    echo "拒绝：无法写入隔离 SSH 配置"
    return 1
  }
  if ! append_projected_ssh_aliases "$tmp"; then
    /bin/rm -f "$tmp" 2>/dev/null || true
    echo "拒绝：无法安全提取系统 SSH Host alias"
    return 1
  fi
  size="$(/usr/bin/stat -f '%z' "$tmp" 2>/dev/null)" || {
    /bin/rm -f "$tmp" 2>/dev/null || true
    echo "拒绝：无法检查隔离 SSH 配置大小"
    return 1
  }
  if [[ ! "$size" =~ ^[0-9]+$ ]] || (( 10#${size} > SSH_STUB_MAX_BYTES )); then
    /bin/rm -f "$tmp" 2>/dev/null || true
    echo "拒绝：隔离 SSH Host alias 索引超出安全上限"
    return 1
  fi
  /bin/mv -f "$tmp" "$SANDBOX_SSH_CONFIG" 2>/dev/null || {
    /bin/rm -f "$tmp" 2>/dev/null || true
    echo "拒绝：无法提交隔离 SSH 配置"
    return 1
  }
  /bin/chmod 600 "$SANDBOX_SSH_CONFIG" 2>/dev/null || {
    echo "拒绝：无法收紧隔离 SSH config 权限"
    return 1
  }
}

remove_sandbox_ssh_config() {
  if [[ ! -e "$SANDBOX_SSH_DIR" && ! -L "$SANDBOX_SSH_DIR" ]]; then
    return 0
  fi
  if path_contains_symlink "$SANDBOX_SSH_DIR" || [[ ! -d "$SANDBOX_SSH_DIR" ]]; then
    echo "拒绝：未授权状态下的隔离 SSH 配置目录不安全"
    return 1
  fi
  if [[ -e "$SANDBOX_SSH_CONFIG" || -L "$SANDBOX_SSH_CONFIG" ]]; then
    if ! is_managed_ssh_stub "$SANDBOX_SSH_CONFIG"; then
      echo "拒绝：未授权状态下存在非 CSSwitch 管理的隔离 SSH config"
      return 1
    fi
    /bin/rm "$SANDBOX_SSH_CONFIG" 2>/dev/null || {
      echo "拒绝：无法撤销隔离 SSH config"
      return 1
    }
  fi
  /bin/rmdir "$SANDBOX_SSH_DIR" 2>/dev/null || true
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --port) PORT="$2"; shift 2;;
    --proxy-url) PROXY_URL="$2"; shift 2;;
    --email) EMAIL="$2"; shift 2;;
    --dry-run) DRY_RUN=1; shift;;
    --skip-oauth-forge) SKIP_FORGE=1; shift;;
    *) echo "未知参数: $1"; exit 1;;
  esac
done

# —— 铁律断言：绝不使用真实目录 / 真实端口 ——
[[ "$PORT" =~ ^[0-9]+$ ]] || { echo "拒绝：端口不是合法整数（$PORT）"; exit 1; }
if (( 10#${PORT} == 8765 )); then echo "拒绝：端口 8765 是真实实例保留端口"; exit 1; fi
if (( 10#${PORT} >= 65535 )); then echo "拒绝：Science 端口必须小于 65535，才能分配隔离预览端口"; exit 1; fi
PREVIEW_PORT=$(( 10#${PORT} + 1 ))
if (( PREVIEW_PORT == 8765 )); then echo "拒绝：预览端口会命中真实实例保留端口 8765"; exit 1; fi
_PROXY_HOSTPORT="$(printf '%s' "$PROXY_URL" | sed -E 's#^[a-zA-Z][a-zA-Z0-9+.-]*://([^/]+).*#\1#')"
_PROXY_PORT="${_PROXY_HOSTPORT##*:}"
if [[ "$_PROXY_PORT" =~ ^[0-9]+$ ]] && (( 10#${_PROXY_PORT} == PREVIEW_PORT )); then
  echo "拒绝：预览端口 $PREVIEW_PORT 与 CSSwitch Gateway 端口冲突"
  exit 1
fi
if [[ "$REUSE_SYSTEM_SSH" != "0" && "$REUSE_SYSTEM_SSH" != "1" ]]; then
  echo "拒绝：系统 SSH 授权值无效"
  exit 1
fi
if [[ "$REUSE_SYSTEM_SSH" == "1" ]]; then
  if [[ ! -f "$SYSTEM_SSH_CONFIG" ]]; then
    echo "拒绝：未找到系统 ~/.ssh/config，不能启用系统 SSH 配置"
    exit 1
  fi
  if ! is_safe_science_bin "$SSH_BRIDGE_BIN"; then
    echo "拒绝：CSSwitch SSH bridge 不存在或不是安全的可执行文件"
    exit 1
  fi
fi
_dd_real="${DATA_DIR:A}"; _real_real="${REAL_DATA_DIR:A}"
if [[ "$_dd_real" == "$_real_real" ]]; then echo "拒绝：data-dir 的真实路径指向真实目录"; exit 1; fi
if path_contains_symlink "$DATA_DIR"; then
  echo "拒绝：Science data-dir 路径包含符号链接"
  exit 1
fi
if [[ "$DRY_RUN" == "1" ]]; then echo "DRY-RUN OK：护栏通过，未启动沙箱。"; exit 0; fi

# The selected runtime owns initialization and migration inside the isolated
# data-dir. Never seed it from the user's real Science data. The backend passes
# SCIENCE_BIN for the installed App or a user-authorized one-shot cache. Without
# that identity, this script may use only the installed App and never an implicit
# data-dir fallback.
mkdir -p "$DATA_DIR"
if path_contains_symlink "$DATA_DIR"; then
  echo "拒绝：Science data-dir 路径在初始化期间发生符号链接变化"
  exit 1
fi
if [[ "$REUSE_SYSTEM_SSH" == "1" ]]; then
  prepare_sandbox_ssh_config
else
  remove_sandbox_ssh_config
fi
BIN_SOURCE="backend-selected runtime"
if [[ -z "$BIN" ]]; then
  BIN="$APP_BIN"
  BIN_SOURCE="official local app"
fi
if ! is_safe_science_bin "$BIN"; then
  echo "拒绝：Science binary 必须是无符号链接的绝对可执行文件"
  exit 1
fi
if [[ "${CSSWITCH_RUNTIME_VERSION_PRECHECKED:-0}" != "1" ]] && ! HOME="$SANDBOX_HOME" "$BIN" --version >/dev/null 2>&1; then
  echo "拒绝：Science binary 未通过非写入版本预检"
  exit 1
fi
unset CSSWITCH_RUNTIME_VERSION_PRECHECKED

if /usr/sbin/lsof -nP -iTCP:"$PREVIEW_PORT" -sTCP:LISTEN -t 2>/dev/null | grep -q .; then
  echo "拒绝：隔离预览端口 $PREVIEW_PORT 已被占用"
  exit 1
fi

# Use a keychain scoped to the isolated HOME.
SANDBOX_KC="$SANDBOX_HOME/Library/Keychains/login.keychain-db"
if [[ ! -f "$SANDBOX_KC" ]]; then
  echo "创建沙箱专属钥匙串（隔离，空密码，不自动锁）…"
  mkdir -p "$SANDBOX_HOME/Library/Keychains"
  if ! HOME="$SANDBOX_HOME" security create-keychain -p "" "$SANDBOX_KC" >/dev/null 2>&1; then
    echo "警告：沙箱专属钥匙串初始化未完成；原始输出因可能含路径而未记录。" >&2
  fi
fi
# 每次启动都确保：加入沙箱搜索表、设为默认、解锁、关自动锁（全部仅作用于沙箱 HOME）
HOME="$SANDBOX_HOME" security list-keychains -d user -s "$SANDBOX_KC" >/dev/null 2>&1 || true
HOME="$SANDBOX_HOME" security default-keychain -d user -s "$SANDBOX_KC" >/dev/null 2>&1 || true
HOME="$SANDBOX_HOME" security unlock-keychain -p "" "$SANDBOX_KC" >/dev/null 2>&1 || true
HOME="$SANDBOX_HOME" security set-keychain-settings "$SANDBOX_KC" >/dev/null 2>&1 || true

# 应用必须先在隔离目录中准备本地状态。
if [[ "$SKIP_FORGE" == "1" ]]; then
  echo "隔离运行状态已由 CSSwitch 准备（路径已隐藏）"
else
  echo "拒绝：请通过 CSSwitch 启动此隔离环境"
  exit 1
fi

echo
echo "启动隔离沙箱 Science（虚拟登录）"
echo "  HOME     = [CSSwitch isolated]"
echo "  data-dir = [CSSwitch isolated Science data]"
echo "  端口     = $PORT   （真实实例 8765 不受影响）"
echo "  预览端口 = $PREVIEW_PORT   （显式固定，供本机 Science 预览使用）"
echo "  二进制   = $BIN_SOURCE"
if [[ "$REUSE_SYSTEM_SSH" == "1" ]]; then
  echo "  系统 SSH = 已显式授权（仅投影 Host alias；连接由 OpenSSH 读取真实配置）"
else
  echo "  系统 SSH = 未授权"
fi
# 掩掉 proxy-url 里的 path secret（一次性鉴权令牌不入日志）
_masked_proxy="$(printf '%s' "$PROXY_URL" | sed -E 's#(://[^/]+/).+#\1****#')"
echo "  推理指向 = $_masked_proxy"
echo "  账号     = $EMAIL （本地假账号，不用真实凭证）"

# Keep local inference traffic on loopback and fail closed for blocked upstreams.
_FASTFAIL_PROXY="http://$_PROXY_HOSTPORT"
_NO_PROXY="127.0.0.1,localhost,::1"
echo "  外联防卡 = Anthropic HTTPS fast-fail（经 $_FASTFAIL_PROXY，no_proxy=$_NO_PROXY）"
echo

if path_contains_symlink "$DATA_DIR"; then
  echo "拒绝：Science data-dir 路径在启动前发生符号链接变化"
  exit 1
fi
typeset -a _SCIENCE_ENV
_SCIENCE_ENV=(
  "HOME=$SANDBOX_HOME"
  "ANTHROPIC_BASE_URL=$PROXY_URL"
  "https_proxy=$_FASTFAIL_PROXY"
  "HTTPS_PROXY=$_FASTFAIL_PROXY"
  "no_proxy=$_NO_PROXY"
  "NO_PROXY=$_NO_PROXY"
)
if [[ "$REUSE_SYSTEM_SSH" == "1" ]]; then
  _SCIENCE_ENV+=(
    "PATH=$SSH_BRIDGE_DIR:${PATH:-/usr/bin:/bin:/usr/sbin:/sbin}"
    "CSSWITCH_SYSTEM_SSH_CONFIG=$SYSTEM_SSH_CONFIG"
  )
fi
if ! /usr/bin/env "${_SCIENCE_ENV[@]}" "$BIN" serve \
    --data-dir "$DATA_DIR" \
    --host 127.0.0.1 \
    --port "$PORT" \
    --sandbox-port "$PREVIEW_PORT" \
    --no-browser --no-auto-update --detached \
    >/dev/null 2>&1; then
  echo "Science 启动命令失败（原始输出可能含临时链接或路径，未写入 CSSwitch 日志）" >&2
  exit 1
fi

echo
echo "已后台启动。验证:"
echo "  健康:   curl -s http://127.0.0.1:$PORT/health || true"
echo "  状态:   请使用 CSSwitch 状态灯确认"
echo "停止:     请使用 CSSwitch「停止全部」"
