#!/usr/bin/env bash
# 运维三件套回归：doctor（只读诊断）/ verify-proxy（校验运行中代理）/ self-test（离线套件包装）。
# 全程不碰真实 ~/.claude-science、不启动 Science、不联网上游。verify-proxy 只打 /health 与
# /v1/models（这两个端点由代理本地作答，不触发任何上游调用，零花费）。
set -u
FAILS=0
ok() { echo "ok - $1"; }
no() { echo "NOT ok - $1"; FAILS=$((FAILS+1)); }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DOCTOR="$ROOT/scripts/doctor.sh"
VERIFY="$ROOT/scripts/verify-proxy.sh"
SELFTEST="$ROOT/scripts/self-test.sh"
CLEAN="$ROOT/scripts/clean-bundle-resources.sh"
PROXY="$ROOT/proxy/csswitch_proxy.py"
T="$(mktemp -d)"
cleanup_bundle_test_artifacts() {
  rm -f "$ROOT/proxy/__pycache__/csswitch-clean-test.pyc" "$ROOT/scripts/csswitch-clean-test.pyc"
  rm -f "$ROOT/proxy/csswitch-clean-test.pyo" "$ROOT/scripts/csswitch-clean-test.pyo"
  rmdir "$ROOT/proxy/__pycache__" 2>/dev/null || true
}
trap cleanup_bundle_test_artifacts EXIT

# ---------- doctor ----------
# 正常：依赖齐全（本机有 python3/node），config 指向不存在的临时路径 → 退出 0
out="$(CSSWITCH_CONFIG="$T/nope.json" SCIENCE_BIN="$T/no-bin" "$DOCTOR" 2>&1)"; rc=$?
if [ $rc -eq 0 ]; then ok "doctor exits 0 when deps present"; else no "doctor failed with deps present (rc=$rc): $out"; fi

# 铁律：代理端口设成 8765 → 必须失败关闭，输出含 8765
out="$(CSSWITCH_PROXY_PORT=8765 CSSWITCH_CONFIG="$T/nope.json" "$DOCTOR" 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "8765"; then ok "doctor fails on reserved port 8765"; else no "doctor did not reject 8765 (rc=$rc): $out"; fi

# key present 契约：app 传 CSSWITCH_KEY_PRESENT=1 + provider/adapter，doctor 报「已配置」且绝不打印任何 key 值
SECRETVAL="DUMMY-KEY-abc123XYZ-should-never-print"
out="$(DEEPSEEK_API_KEY="$SECRETVAL" CSSWITCH_PROVIDER=deepseek CSSWITCH_ADAPTER=deepseek CSSWITCH_KEY_PRESENT=1 CSSWITCH_CONFIG="$T/nope.json" "$DOCTOR" 2>&1)"; rc=$?
if echo "$out" | grep -q "$SECRETVAL"; then no "doctor LEAKED key value"; else ok "doctor never prints key value"; fi
if echo "$out" | grep -q "已配置"; then ok "doctor reports key present (已配置)"; else no "doctor did not report key present: $out"; fi
# 反面：不传 KEY_PRESENT → 应报「尚未填 key」，不得报「已配置」
out2="$(CSSWITCH_PROVIDER=deepseek CSSWITCH_ADAPTER=deepseek CSSWITCH_CONFIG="$T/nope.json" "$DOCTOR" 2>&1)"
if echo "$out2" | grep -q "尚未填 key"; then ok "doctor reports key absent when KEY_PRESENT unset"; else no "doctor absent-key wording drift: $out2"; fi

# config 权限：0644 → 警告应为 600（不改变退出码，仍 0）
CFG644="$T/cfg644.json"; echo '{}' > "$CFG644"; chmod 644 "$CFG644"
out="$(CSSWITCH_CONFIG="$CFG644" "$DOCTOR" 2>&1)"; rc=$?
if echo "$out" | grep -q "600"; then ok "doctor warns on non-600 config perms"; else no "doctor missed bad config perms: $out"; fi

# config 是符号链接 → 拒绝（失败关闭）
CFGLINK="$T/cfglink.json"; ln -s "$CFG644" "$CFGLINK"
out="$(CSSWITCH_CONFIG="$CFGLINK" "$DOCTOR" 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接"; then ok "doctor rejects symlinked config"; else no "doctor accepted symlinked config (rc=$rc): $out"; fi

# ---------- verify-proxy ----------
if [ "$(python3 "$ROOT/test/_capability.py")" != "1" ]; then
  echo "skip - verify-proxy 段 env-blocked（loopback 被禁，无法起临时代理）"
else
# 找一个空闲端口，起一个真代理（假 key，上游 URL 是假的但不会被 /health、/v1/models 触及）
P="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
SEC="verify-test-secret"
DEEPSEEK_API_KEY=fake CSSWITCH_UPSTREAM_URL="http://127.0.0.1:1/never" \
  python3 "$PROXY" --provider deepseek --port "$P" --auth-token "$SEC" \
  >/dev/null 2>&1 &
PROXY_PID=$!
# 等健康
up=0
for _ in $(seq 1 50); do
  if curl -s -m 2 "http://127.0.0.1:$P/$SEC/health" 2>/dev/null | grep -q '"ok"'; then up=1; break; fi
  sleep 0.1
done
if [ "$up" = "1" ]; then ok "test proxy came up"; else no "test proxy did not start"; fi

out="$("$VERIFY" --port "$P" --secret "$SEC" 2>&1)"; rc=$?
if [ $rc -eq 0 ] && echo "$out" | grep -q "代理校验通过"; then ok "verify-proxy passes on healthy proxy"; else no "verify-proxy failed on healthy proxy (rc=$rc): $out"; fi

out="$("$VERIFY" --port "$P" 2>&1)"; rc=$?
if [ $rc -ne 0 ]; then ok "verify-proxy fails without required secret (403)"; else no "verify-proxy passed without secret (rc=$rc): $out"; fi

kill "$PROXY_PID" 2>/dev/null; wait "$PROXY_PID" 2>/dev/null
out="$("$VERIFY" --port "$P" --secret "$SEC" 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "✗"; then ok "verify-proxy fails when proxy is down"; else no "verify-proxy passed with proxy down (rc=$rc): $out"; fi
fi   # end verify-proxy loopback-gate

# ---------- self-test ----------
# 只做静态检查（不实跑，避免和 run_all 递归）：可执行 + 委派给 run_all.sh
if [ -x "$SELFTEST" ]; then ok "self-test.sh is executable"; else no "self-test.sh not executable"; fi
if grep -q "run_all.sh" "$SELFTEST"; then ok "self-test delegates to run_all.sh"; else no "self-test does not delegate to run_all.sh"; fi

# ---------- bundle resource cleanup ----------
mkdir -p "$ROOT/proxy/__pycache__"
: > "$ROOT/proxy/__pycache__/csswitch-clean-test.pyc"
: > "$ROOT/scripts/csswitch-clean-test.pyc"
: > "$ROOT/proxy/csswitch-clean-test.pyo"
: > "$ROOT/scripts/csswitch-clean-test.pyo"
out="$("$CLEAN" 2>&1)"; rc=$?
if [ $rc -eq 0 ]; then ok "clean-bundle-resources exits 0"; else no "clean-bundle-resources failed (rc=$rc): $out"; fi
if [ ! -e "$ROOT/proxy/__pycache__/csswitch-clean-test.pyc" ] \
  && [ ! -e "$ROOT/scripts/csswitch-clean-test.pyc" ] \
  && [ ! -e "$ROOT/proxy/csswitch-clean-test.pyo" ] \
  && [ ! -e "$ROOT/scripts/csswitch-clean-test.pyo" ]; then
  ok "clean-bundle-resources removes pycache artifacts from bundled dirs"
else
  no "clean-bundle-resources left pycache artifacts behind"
fi

echo "----"
if [ $FAILS -eq 0 ]; then echo "ALL PASS"; exit 0; else echo "$FAILS FAILED"; exit 1; fi
