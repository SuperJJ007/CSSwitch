#!/bin/zsh
# 启动一个【隔离、无登录】的 Claude Science 沙箱，推理指向本项目翻译代理。
#
# 铁律保障（见 CLAUDE.md）:
#   - 独立 HOME + 独立 data-dir + 独立端口，绝不修改/删除真实 ~/.claude-science，绝不用端口 8765
#   - 只从真实 ~/.claude-science 只读 APFS 克隆运行时资产（bin/conda/runtime/seed-assets），绝不复制任何登录凭证
#     （.oauth-tokens / encryption.key / active-org.json / orgs / .key-backups）
#   - 因此沙箱启动即【未登录】。要推理需在沙箱里【全新独立登录】（对真实登录零影响）
#
# 用法:
#   scripts/launch-science-sandbox.sh [--port 8990] [--proxy-url http://127.0.0.1:18991]
#   先在另一个终端起代理: DASHSCOPE_API_KEY=... python3 proxy/qwen_proxy.py --port 18991
set -euo pipefail

PROJ="${0:A:h:h}"
SANDBOX_HOME="$PROJ/.sandbox/home"
DATA_DIR="$SANDBOX_HOME/.claude-science"
REAL_DIR="$HOME/.claude-science"
APP="/Applications/Claude Science.app/Contents/MacOS/ClaudeScience"
BIN="/Applications/Claude Science.app/Contents/Resources/bin/claude-science"
PORT=8990
PROXY_URL="http://127.0.0.1:18991"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --port) PORT="$2"; shift 2;;
    --proxy-url) PROXY_URL="$2"; shift 2;;
    *) echo "未知参数: $1"; exit 1;;
  esac
done

# —— 铁律断言：绝不使用真实目录 / 真实端口 ——
_dd="${DATA_DIR:A}"; _rd="${REAL_DIR:A}"
if [[ "$_dd" == "$_rd" ]]; then echo "拒绝：data-dir 的真实路径指向真实目录"; exit 1; fi
[[ "$PORT" =~ ^[0-9]+$ ]] || { echo "拒绝：端口不是合法整数（$PORT）"; exit 1; }
if (( 10#${PORT} == 8765 )); then echo "拒绝：端口 8765 是真实实例保留端口"; exit 1; fi

# —— 首次：克隆运行时资产，绝不复制登录凭证 ——
if [[ ! -d "$DATA_DIR/bin" ]]; then
  echo "首次初始化沙箱运行时（APFS 克隆，只拷运行时、不拷登录）…"
  mkdir -p "$DATA_DIR"
  for asset in bin conda runtime seed-assets; do
    if [[ -d "$REAL_DIR/$asset" ]]; then
      cp -Rc "$REAL_DIR/$asset" "$DATA_DIR/$asset"
    fi
  done
  # 明确不拷：.oauth-tokens encryption.key active-org.json orgs .key-backups install-id
  echo "运行时就绪。沙箱为【未登录】状态。"
fi

# —— 若沙箱不慎混入登录凭证，直接拦停（双保险）——
for secret in .oauth-tokens encryption.key active-org.json orgs .key-backups; do
  if [[ -e "$DATA_DIR/$secret" ]]; then
    echo "拒绝启动：沙箱内出现登录凭证 '$secret'，违反铁律。请删除后重试。"; exit 1
  fi
done

echo "启动隔离沙箱 Science"
echo "  HOME     = $SANDBOX_HOME"
echo "  data-dir = $DATA_DIR"
echo "  端口     = $PORT   （真实实例 8765 不受影响）"
echo "  推理指向 = $PROXY_URL"
echo

HOME="$SANDBOX_HOME" \
ANTHROPIC_BASE_URL="$PROXY_URL" \
"$BIN" serve \
  --data-dir "$DATA_DIR" \
  --port "$PORT" \
  --no-browser --no-auto-update --detached

echo
echo "已后台启动。下一步（需你手动完成，Claude 不代做登录）:"
echo "  1) 取登录链接: HOME='$SANDBOX_HOME' '$BIN' url --data-dir '$DATA_DIR'"
echo "  2) 浏览器打开该链接，在沙箱里【全新登录】（另一套会话，真实登录不受影响）"
echo "  3) 登录后发消息，推理即经代理走通义千问"
echo "停止: scripts/stop-science-sandbox.sh"
