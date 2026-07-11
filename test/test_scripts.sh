#!/usr/bin/env bash
set -u
FAILS=0
ok() { echo "ok - $1"; }
no() { echo "NOT ok - $1"; FAILS=$((FAILS+1)); }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# 7.6 停止脚本如实报告
T="$(mktemp -d)"
T="$(cd "$T" && pwd -P)"
OUTER_HOME="$T/outerhome"
mkdir -p "$OUTER_HOME/.claude-science"
mkdir -p "$T/home/.claude-science"           # DATA_DIR 存在，走到 stop 调用
FAKE_FAIL="$T/fake-fail"; printf '#!/bin/sh\nexit 1\n' > "$FAKE_FAIL"; chmod +x "$FAKE_FAIL"
FAKE_OK="$T/fake-ok";     printf '#!/bin/sh\nexit 0\n' > "$FAKE_OK";   chmod +x "$FAKE_OK"

out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/home" SCIENCE_BIN="$FAKE_FAIL" "$ROOT/scripts/stop-science-sandbox.sh" 2>&1)"; rc=$?
if [ $rc -ne 0 ]; then ok "stop reports failure rc!=0"; else no "stop hid failure (rc=$rc)"; fi
if echo "$out" | grep -q "沙箱已停"; then no "stop falsely claimed success"; else ok "stop did not falsely claim success"; fi

out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/home" SCIENCE_BIN="$FAKE_OK" "$ROOT/scripts/stop-science-sandbox.sh" 2>&1)"; rc=$?
if [ $rc -eq 0 ] && echo "$out" | grep -q "沙箱已停"; then ok "stop reports success on rc=0"; else no "stop mis-reported success path (rc=$rc)"; fi

FAKE_LINK="$T/fake-link"
ln -s "$FAKE_OK" "$FAKE_LINK"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/home" SCIENCE_BIN="$FAKE_LINK" "$ROOT/scripts/stop-science-sandbox.sh" 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接"; then ok "stop rejects explicit Science symlink"; else no "stop accepted explicit Science symlink (rc=$rc): $out"; fi

mkdir -p "$T/real-bin-parent"
cp "$FAKE_OK" "$T/real-bin-parent/claude-science"
ln -s "$T/real-bin-parent" "$T/linked-bin-parent"
PARENT_LINK_BIN="$T/linked-bin-parent/claude-science"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/home" SCIENCE_BIN="$PARENT_LINK_BIN" "$ROOT/scripts/stop-science-sandbox.sh" 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接"; then ok "stop rejects symlinked Science parent"; else no "stop accepted symlinked Science parent (rc=$rc): $out"; fi

mkdir -p "$T/realhome/.claude-science"
out="$(HOME="$T/realhome" SANDBOX_HOME="$T/realhome" SCIENCE_BIN="$FAKE_OK" "$ROOT/scripts/stop-science-sandbox.sh" 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "真实目录"; then ok "stop rejects real data-dir collision"; else no "stop allowed real data-dir collision (rc=$rc): $out"; fi

mkdir -p "$T/linkhome"
ln -s "$OUTER_HOME/.claude-science" "$T/linkhome/.claude-science"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/linkhome" SCIENCE_BIN="$FAKE_OK" "$ROOT/scripts/stop-science-sandbox.sh" 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "真实目录"; then ok "stop rejects symlinked real data-dir collision"; else no "stop allowed symlinked real data-dir collision (rc=$rc): $out"; fi

# 7.7 端口归一化 + dry-run
out="$(SANDBOX_HOME="$T/vh" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 08765 --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "拒绝"; then ok "08765 rejected via int-normalize"; else no "08765 bypassed guard (rc=$rc)"; fi

out="$(SANDBOX_HOME="$T/vh" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9931 --dry-run 2>&1)"; rc=$?
if [ $rc -eq 0 ] && echo "$out" | grep -q "DRY-RUN OK"; then ok "valid port passes guards in dry-run"; else no "valid port dry-run failed (rc=$rc): $out"; fi

out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/vh-link" SCIENCE_BIN="$FAKE_LINK" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9932 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接"; then ok "launch rejects explicit Science symlink"; else no "launch accepted explicit Science symlink (rc=$rc): $out"; fi

out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/vh-parent-link" SCIENCE_BIN="$PARENT_LINK_BIN" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9933 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接"; then ok "launch rejects symlinked Science parent"; else no "launch accepted symlinked Science parent (rc=$rc): $out"; fi

# 7.7 review: 畸形端口必须失败关闭（fail-closed），而不是绕过算术守卫
out="$(SANDBOX_HOME="$T/vh2" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 8765x --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "拒绝"; then ok "malformed port 8765x rejected fail-closed"; else no "malformed port 8765x slipped guard (rc=$rc): $out"; fi

echo "----"
if [ $FAILS -eq 0 ]; then echo "ALL PASS"; exit 0; else echo "$FAILS FAILED"; exit 1; fi
