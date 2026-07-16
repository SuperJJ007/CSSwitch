#!/usr/bin/env bash
# CSSwitch doctor：只读环境诊断。
#   - 不启动任何进程、不联网、绝不读写/删除真实 ~/.claude-science。
#   - 绝不打印任何 provider key 的值（只报 present/absent）。
#   - 端口命中真实实例保留端口 8765 直接失败（铁律）。
# 覆盖变量（便于测试与自定义）：
#   CSSWITCH_PROVIDER (生效 template_id，如 deepseek/qwen/glm/xiaomi/…)
#   CSSWITCH_ADAPTER  (deepseek|qwen|relay|codex)   CSSWITCH_KEY_PRESENT (0|1)
#   CSSWITCH_AUTH_MODE (api_key|keychain_oauth|none)
#   CSSWITCH_PROXY_PORT  CSSWITCH_SANDBOX_PORT  CSSWITCH_CONFIG (config.json 路径)
#   CSSWITCH_GATEWAY_BIN  SCIENCE_BIN
#   CSSWITCH_DOCTOR_CHECK_REAL_HOME=1  显式 opt-in 后才检查 $HOME/.claude-science 是否存在
set -u

PROVIDER="${CSSWITCH_PROVIDER:-}"
ADAPTER="${CSSWITCH_ADAPTER:-}"
AUTH_MODE="${CSSWITCH_AUTH_MODE:-api_key}"
KEY_PRESENT="${CSSWITCH_KEY_PRESENT:-0}"
PROXY_PORT="${CSSWITCH_PROXY_PORT:-18991}"
SANDBOX_PORT="${CSSWITCH_SANDBOX_PORT:-8990}"
CONFIG="${CSSWITCH_CONFIG:-$HOME/.csswitch/config.json}"
SCIENCE_BIN="${SCIENCE_BIN:-/Applications/Claude Science.app/Contents/Resources/bin/claude-science}"
GATEWAY_BIN="${CSSWITCH_GATEWAY_BIN:-}"
CHECK_REAL_HOME="${CSSWITCH_DOCTOR_CHECK_REAL_HOME:-0}"

WARN=0; FAIL=0
pass() { echo "  ✓ $1"; }
warn() { echo "  ⚠ $1"; WARN=$((WARN + 1)); }
fail() { echo "  ✗ $1"; FAIL=$((FAIL + 1)); }

echo "CSSwitch doctor（只读诊断，不启动进程、不联网、绝不碰真实目录）"
echo "生效来源=${PROVIDER:-（无）}  适配器=${ADAPTER:-（无）}  代理端口=$PROXY_PORT  沙箱端口=$SANDBOX_PORT"

echo "[依赖]"
if [ -n "$GATEWAY_BIN" ] && [ -x "$GATEWAY_BIN" ]; then pass "Rust gateway 可执行：$GATEWAY_BIN"; else warn "未传入可执行 CSSWITCH_GATEWAY_BIN（app 自检会自动注入安装包 sidecar）"; fi
# The packaged app does not require Node.js.

echo "[Science 二进制]"
if [ -x "$SCIENCE_BIN" ]; then pass "找到 $SCIENCE_BIN"; else warn "未找到 Science 二进制（一键越登录需要）：$SCIENCE_BIN"; fi

echo "[生效配置]"
# 多 profile：app 传来 template_id + adapter + 认证类型，以及 API-key profile 的 key 有无。
# OAuth 凭据绝不进入 config.json 或脚本环境；脱敏状态由 app 另列在 [Codex 实验]。
if [ -z "$PROVIDER" ]; then
  warn "当前没有「生效」配置（在面板点「设为当前」选一条）"
elif [ "$AUTH_MODE" = "keychain_oauth" ]; then
  pass "生效来源：${PROVIDER}（${ADAPTER:-?} 适配器）· CSSwitch Keychain OAuth（脱敏状态见 [Codex 实验]）"
elif [ "$AUTH_MODE" = "none" ]; then
  pass "生效来源：${PROVIDER}（${ADAPTER:-?} 适配器）· 无需凭据"
elif [ "$KEY_PRESENT" = "1" ]; then
  pass "生效来源：${PROVIDER}（${ADAPTER:-?} 适配器）· key 已配置在 config.json（值不显示）"
else
  warn "生效来源：${PROVIDER}（${ADAPTER:-?} 适配器）· 尚未填 key（在面板该配置里粘贴）"
fi

echo "[端口]"
classify_port() {  # $1=port；打印占用分型
  local p="$1" cmd
  if [ "$((10#$p))" -eq 8765 ]; then echo "保留端口 8765（真实实例，绝不占用/干预）"; return; fi
  cmd="$(lsof -nP -iTCP:"$p" -sTCP:LISTEN 2>/dev/null | awk 'NR==2{print $1" (pid "$2")"}')"
  if [ -z "$cmd" ]; then echo "空闲"; return; fi
  local lc
  lc="$(printf '%s' "$cmd" | tr '[:upper:]' '[:lower:]')"
  case "$lc" in
    *python*|*csswitch*|*claude-science*) echo "疑似 CSSwitch 旧进程：${cmd}（可 stop_all 或手动清）" ;;
    *) echo "未知占用：$cmd" ;;
  esac
}
for p in "$PROXY_PORT" "$SANDBOX_PORT"; do
  if ! [[ "$p" =~ ^[0-9]+$ ]]; then fail "端口非法整数：$p"; continue; fi
  if [ "$((10#$p))" -eq 8765 ]; then fail "端口 $p 命中真实实例保留端口 8765（铁律禁用）"; continue; fi
  detail="$(classify_port "$p")"
  if [ "$detail" = "空闲" ]; then pass "端口 $p 空闲"; else warn "端口 $p 占用分型：$detail"; fi
done

echo "[本地配置]"
if [ -L "$CONFIG" ]; then
  fail "config 是符号链接（拒绝，防跟随写到别处）：$CONFIG"
elif [ -e "$CONFIG" ]; then
  perm="$(stat -f '%Lp' "$CONFIG" 2>/dev/null || stat -c '%a' "$CONFIG" 2>/dev/null)"
  if [ "$perm" = "600" ]; then pass "config 存在且权限 600：$CONFIG"; else warn "config 权限为 ${perm}，应为 600：$CONFIG"; fi
else
  warn "config 不存在（首次运行 GUI 会创建）：$CONFIG"
fi

echo "[铁律]"
if [ "$CHECK_REAL_HOME" = "1" ]; then
  REAL_DIR="$HOME/.claude-science"
  if [ -d "$REAL_DIR" ]; then pass "真实目录存在（显式 opt-in 只读检查，绝不写/删）：$REAL_DIR"; else warn "未见真实 Science 目录（显式 opt-in 只读检查）：$REAL_DIR"; fi
else
  pass "真实 HOME 检查默认跳过（未设置 CSSWITCH_DOCTOR_CHECK_REAL_HOME=1，不读取 ~/.claude-science）"
fi

echo "----"
echo "诊断完成：警告 ${WARN}，失败 ${FAIL}"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
