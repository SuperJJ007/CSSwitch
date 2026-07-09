#!/bin/zsh
# 启动一个【隔离 + 虚拟登录】的 Claude Science 沙箱：
#   用本地自造的虚拟 OAuth 让 Science 认为已登录（virtual@localhost.invalid），
#   推理经 ANTHROPIC_BASE_URL 导去本项目翻译代理 → 通义千问。
#   推理零 Anthropic、零真实凭证；但启动阶段 Science 自身仍可能尝试访问其硬编码的
#   profile/account 接口（api.anthropic.com），该请求失败不影响使用。故不宣称
#   「完全零 Anthropic 接触」这类绝对说法（与 README「免责声明」一致）。
#
# 铁律保障（见 CLAUDE.md）:
#   - 独立 HOME + 独立 data-dir + 独立端口，绝不修改/删除真实 ~/.claude-science，绝不用端口 8765
#   - 只从真实 ~/.claude-science 只读 APFS 克隆运行时资产（bin/conda/runtime/seed-assets），绝不复制任何真实登录凭证或用户数据
#   - 写入沙箱的是【自造的假凭证】(make-virtual-oauth.mjs)，与真实 OAuth 无关
#   - encryption.key 的 keychain 镜像账号按【路径哈希】派生，沙箱与真实天然隔离
#
# 用法:
#   先起代理: DEEPSEEK_API_KEY=... python3 proxy/csswitch_proxy.py --provider deepseek --port 18991
#   再起沙箱: scripts/launch-virtual-sandbox.sh [--port 8990] [--proxy-url http://127.0.0.1:18991]
set -euo pipefail

PROJ="${0:A:h:h}"
SANDBOX_HOME="${SANDBOX_HOME:-$PROJ/.sandbox/home}"
DATA_DIR="$SANDBOX_HOME/.claude-science"   # = auth_dir（Science 按 HOME 推导）
REAL_DIR="$HOME/.claude-science"
APP_BIN="/Applications/Claude Science.app/Contents/Resources/bin/claude-science"
BIN="${SCIENCE_BIN:-}"
PORT=8990
PROXY_URL="http://127.0.0.1:18991"
EMAIL="virtual@localhost.invalid"
DRY_RUN=0
SKIP_FORGE=0   # app 调用时置 1：OAuth 由 app 进程内 Rust 伪造，本脚本不再调 node

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
_dd_real="${DATA_DIR:A}"; _real_real="${REAL_DIR:A}"
if [[ "$_dd_real" == "$_real_real" ]]; then echo "拒绝：data-dir 的真实路径指向真实目录"; exit 1; fi
if [[ "$DRY_RUN" == "1" ]]; then echo "DRY-RUN OK：护栏通过，未启动沙箱。"; exit 0; fi

# —— 首次：克隆运行时资产，绝不复制真实登录凭证 ——
if [[ ! -d "$DATA_DIR/bin" ]]; then
  echo "首次初始化沙箱运行时（APFS 克隆，只拷运行时、不拷真实登录）…"
  mkdir -p "$DATA_DIR"
  for asset in bin conda runtime seed-assets; do
    if [[ -d "$REAL_DIR/$asset" ]]; then
      cp -Rc "$REAL_DIR/$asset" "$DATA_DIR/$asset"
    fi
  done
  echo "运行时就绪。"
fi

# 优先级：显式 SCIENCE_BIN > 沙箱内已克隆 runtime > App 内置 binary。
# 沙箱内 runtime 优先，App 内置 binary 仅作缺省 fallback。
if [[ -z "$BIN" ]]; then
  if [[ -x "$DATA_DIR/bin/claude-science" ]]; then
    BIN="$DATA_DIR/bin/claude-science"
  else
    BIN="$APP_BIN"
  fi
fi
if [[ ! -x "$BIN" ]]; then echo "找不到 Science 二进制: $BIN"; exit 1; fi

# —— 沙箱专属钥匙串（消除「找不到钥匙串」弹窗）——
# Science 会把 encryption.key 镜像进 macOS 钥匙串。沙箱 HOME 下没有任何钥匙串，
# securityd 报「找不到默认钥匙串」→ 反复弹「还原为默认」窗。这里在【沙箱 HOME 内】
# 建一个独立、空密码、不自动锁的 login.keychain-db，并只在 HOME=$SANDBOX_HOME 的
# 上下文里设为默认。真实登录钥匙串（~/Library/Keychains）零改动、零接触。
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

# —— 写入自造的虚拟 OAuth（每次覆盖，保持唯一 .enc；复用已有 encryption.key）——
# 注意：不覆盖 HOME —— 伪造器要用【真实】HOME 判断是否误写真实凭证目录（护栏）。
# app 一键流程走 --skip-oauth-forge：OAuth 已由 app 进程内 Rust 原生伪造（src/oauth_forge.rs），
# 打包 app 从此零 node。独立/dev 运行本脚本（不带该 flag）仍用 .mjs 伪造（需 node）。
if [[ "$SKIP_FORGE" == "1" ]]; then
  echo "虚拟 OAuth 由 app 进程内写入（Rust 原生，已跳过 node 伪造）→ $DATA_DIR"
else
  echo "写入虚拟 OAuth 凭证（node）→ $DATA_DIR"
  node "$PROJ/scripts/make-virtual-oauth.mjs" --auth-dir "$DATA_DIR" --email "$EMAIL"
fi

echo
echo "启动隔离沙箱 Science（虚拟登录）"
echo "  HOME     = $SANDBOX_HOME"
echo "  data-dir = $DATA_DIR"
echo "  端口     = $PORT   （真实实例 8765 不受影响）"
echo "  二进制   = $BIN"
# 掩掉 proxy-url 里的 path secret（一次性鉴权令牌不入日志）
_masked_proxy="$(printf '%s' "$PROXY_URL" | sed -E 's#(://[^/]+/).+#\1****#')"
echo "  推理指向 = $_masked_proxy"
echo "  账号     = $EMAIL （本地假账号，不用真实凭证）"

# #3 修复：沙箱到 Anthropic 域名（claude.ai / *.claude.com / *.anthropic.com）的外联，
# 经本地代理 fast-fail。不接这一步，启动时对 claude.ai/api/oauth/profile 的【阻塞】请求
# 在到不了 claude.ai 的网络上会挂住重试 → UI 卡在 "Switching organization"。
# 做法：**只设 https_proxy**（那条卡死的 profile 请求是 HTTPS → 走 CONNECT，代理的
# do_CONNECT 对上述域名立即 401，让客户端快速判定未登录；其余 HTTPS 隧道透传）。
# 【刻意不设 http_proxy】：代理未实现普通 HTTP 转发（GET http://host/…），若设了 http_proxy
# 普通 HTTP 的 MCP/下载/包源会撞代理拿 404（修 P2）；不设则普通 HTTP 直连或走用户自己的
# http_proxy，且无 Anthropic 域名走普通 HTTP，故不影响 fast-fail。
# no_proxy 让本地推理仍直连 127.0.0.1（operon 认【小写】 https_proxy/no_proxy）。
_PROXY_HOSTPORT="$(printf '%s' "$PROXY_URL" | sed -E 's#^[a-zA-Z][a-zA-Z0-9+.-]*://([^/]+).*#\1#')"
_FASTFAIL_PROXY="http://$_PROXY_HOSTPORT"
_NO_PROXY="127.0.0.1,localhost,::1"
echo "  外联防卡 = Anthropic HTTPS fast-fail（经 $_FASTFAIL_PROXY，no_proxy=$_NO_PROXY）"
echo

HOME="$SANDBOX_HOME" \
ANTHROPIC_BASE_URL="$PROXY_URL" \
https_proxy="$_FASTFAIL_PROXY" HTTPS_PROXY="$_FASTFAIL_PROXY" \
no_proxy="$_NO_PROXY" NO_PROXY="$_NO_PROXY" \
"$BIN" serve \
  --data-dir "$DATA_DIR" \
  --port "$PORT" \
  --no-browser --no-auto-update --detached

echo
echo "已后台启动。验证:"
echo "  健康:   curl -s http://127.0.0.1:$PORT/health || true"
echo "  状态:   HOME='$SANDBOX_HOME' '$BIN' status --data-dir '$DATA_DIR'"
echo "停止:     scripts/stop-science-sandbox.sh   （data-dir 已改为虚拟沙箱同一路径）"
