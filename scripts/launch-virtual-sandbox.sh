#!/bin/zsh
# 启动 CSSwitch 管理的隔离运行环境。
# Safety boundaries:
#   - 独立 HOME + 独立 data-dir + 独立端口，绝不修改/删除真实 ~/.claude-science，绝不用端口 8765
#   - 不读取或复制真实 Science 数据；隔离 data-dir 只由官方 App runtime 管理
#   - 只使用应用在隔离目录中生成的本地状态，与真实账号无关
#   - 使用独立的本地钥匙串
#
# 用法:
#   代理由 CSSwitch 桌面端启动并管理；本脚本只负责虚拟 Science 沙箱。
#   再起沙箱: scripts/launch-virtual-sandbox.sh [--port 8990] [--proxy-url http://127.0.0.1:18991]
set -euo pipefail

PROJ="${0:A:h:h}"
SANDBOX_HOME="${SANDBOX_HOME:-$PROJ/.sandbox/home}"
DATA_DIR="$SANDBOX_HOME/.claude-science"   # = auth_dir（Science 按 HOME 推导）
REAL_DATA_DIR="$HOME/.claude-science"
APP_BIN="/Applications/Claude Science.app/Contents/Resources/bin/claude-science"
BIN="${SCIENCE_BIN:-}"
PORT=8990
PROXY_URL="http://127.0.0.1:18991"
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
_dd_real="${DATA_DIR:A}"; _real_real="${REAL_DATA_DIR:A}"
if [[ "$_dd_real" == "$_real_real" ]]; then echo "拒绝：data-dir 的真实路径指向真实目录"; exit 1; fi
if [[ "$DRY_RUN" == "1" ]]; then echo "DRY-RUN OK：护栏通过，未启动沙箱。"; exit 0; fi

# The selected official/fallback binary owns initialization and runtime migration
# inside the isolated data-dir. Never seed it from the user's real Science data.
mkdir -p "$DATA_DIR"

# 优先级：显式 SCIENCE_BIN（测试/开发）> 本机官方 App > 旧沙箱 binary fallback。
# CSSwitch 不调用 Science 自更新、不下载或覆盖 binary；官方候选预检失败才回退。
BIN_SOURCE="explicit override"
if [[ -n "$BIN" ]]; then
  if ! is_safe_science_bin "$BIN"; then
    echo "拒绝：显式 SCIENCE_BIN 必须是无符号链接的绝对可执行文件"
    exit 1
  fi
  if ! HOME="$SANDBOX_HOME" "$BIN" --version >/dev/null 2>&1; then
    echo "拒绝：显式 SCIENCE_BIN 未通过非写入版本预检"
    exit 1
  fi
else
  if is_safe_science_bin "$APP_BIN" && HOME="$SANDBOX_HOME" "$APP_BIN" --version >/dev/null 2>&1; then
    BIN="$APP_BIN"
    BIN_SOURCE="official local app"
  elif is_safe_science_bin "$DATA_DIR/bin/claude-science" && HOME="$SANDBOX_HOME" "$DATA_DIR/bin/claude-science" --version >/dev/null 2>&1; then
    BIN="$DATA_DIR/bin/claude-science"
    BIN_SOURCE="retained sandbox fallback"
  fi
fi
if [[ -z "$BIN" ]]; then echo "找不到通过安全预检的 Science 二进制"; exit 1; fi

if /usr/sbin/lsof -nP -iTCP:"$PREVIEW_PORT" -sTCP:LISTEN -t 2>/dev/null | grep -q .; then
  echo "拒绝：隔离预览端口 $PREVIEW_PORT 已被占用"
  exit 1
fi

# Use a keychain scoped to the isolated HOME.
SANDBOX_KC="$SANDBOX_HOME/Library/Keychains/login.keychain-db"
if [[ ! -f "$SANDBOX_KC" ]]; then
  echo "创建沙箱专属钥匙串（隔离，空密码，不自动锁）…"
  mkdir -p "$SANDBOX_HOME/Library/Keychains"
  HOME="$SANDBOX_HOME" security create-keychain -p "" "$SANDBOX_KC" || true
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
echo "  预览端口 = $PREVIEW_PORT   （显式固定，供本机或 SSH 隧道访问）"
echo "  二进制   = $BIN_SOURCE"
# 掩掉 proxy-url 里的 path secret（一次性鉴权令牌不入日志）
_masked_proxy="$(printf '%s' "$PROXY_URL" | sed -E 's#(://[^/]+/).+#\1****#')"
echo "  推理指向 = $_masked_proxy"
echo "  账号     = $EMAIL （本地假账号，不用真实凭证）"

# Keep local inference traffic on loopback and fail closed for blocked upstreams.
_FASTFAIL_PROXY="http://$_PROXY_HOSTPORT"
_NO_PROXY="127.0.0.1,localhost,::1"
echo "  外联防卡 = Anthropic HTTPS fast-fail（经 $_FASTFAIL_PROXY，no_proxy=$_NO_PROXY）"
echo

if ! HOME="$SANDBOX_HOME" \
  ANTHROPIC_BASE_URL="$PROXY_URL" \
  https_proxy="$_FASTFAIL_PROXY" HTTPS_PROXY="$_FASTFAIL_PROXY" \
  no_proxy="$_NO_PROXY" NO_PROXY="$_NO_PROXY" \
  "$BIN" serve \
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
