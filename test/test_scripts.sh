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

mkdir -p "$T/outside-data" "$T/arbitrary-linkhome"
ln -s "$T/outside-data" "$T/arbitrary-linkhome/.claude-science"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/arbitrary-linkhome" SCIENCE_BIN="$FAKE_OK" "$ROOT/scripts/stop-science-sandbox.sh" 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接"; then ok "stop rejects arbitrary symlinked data-dir"; else no "stop followed arbitrary symlinked data-dir (rc=$rc): $out"; fi

# 7.7 端口归一化 + dry-run
out="$(SANDBOX_HOME="$T/vh" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 08765 --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "拒绝"; then ok "08765 rejected via int-normalize"; else no "08765 bypassed guard (rc=$rc)"; fi

out="$(SANDBOX_HOME="$T/vh" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9931 --dry-run 2>&1)"; rc=$?
if [ $rc -eq 0 ] && echo "$out" | grep -q "DRY-RUN OK"; then ok "valid port passes guards in dry-run"; else no "valid port dry-run failed (rc=$rc): $out"; fi

out="$(SANDBOX_HOME="$T/vh" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 8764 --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "预览端口"; then ok "preview port cannot collide with reserved 8765"; else no "preview port reached reserved 8765 (rc=$rc): $out"; fi

out="$(SANDBOX_HOME="$T/vh" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 65535 --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "小于 65535"; then ok "preview port overflow rejected"; else no "preview port overflow accepted (rc=$rc): $out"; fi

out="$(SANDBOX_HOME="$T/vh" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9931 --proxy-url http://127.0.0.1:9932/path-secret --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "Gateway 端口冲突"; then ok "preview port cannot collide with Gateway"; else no "preview/Gateway collision accepted (rc=$rc): $out"; fi

mkdir -p "$T/no-ssh-home"
out="$(HOME="$T/no-ssh-home" SANDBOX_HOME="$T/vh-no-ssh" CSSWITCH_REUSE_SYSTEM_SSH=1 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9935 --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "未找到系统 ~/.ssh/config"; then ok "SSH bridge fails closed without system config"; else no "SSH bridge accepted missing system config (rc=$rc): $out"; fi

BRIDGE_HOME="$T/bridge-home"
mkdir -p "$BRIDGE_HOME/.ssh"
printf 'Host bridge-alias\n  HostName 127.0.0.1\n  Port 2222\n' > "$BRIDGE_HOME/.ssh/config"
out="$(CSSWITCH_SYSTEM_SSH_CONFIG="$BRIDGE_HOME/.ssh/config" "$ROOT/scripts/ssh-bridge/ssh" -G bridge-alias 2>/dev/null)"; rc=$?
if [ $rc -eq 0 ] && echo "$out" | grep -qx 'hostname 127.0.0.1' && echo "$out" | grep -qx 'port 2222'; then ok "SSH bridge injects the authorized config with -F"; else no "SSH bridge did not load authorized config (rc=$rc)"; fi
out="$(env -u CSSWITCH_SYSTEM_SSH_CONFIG "$ROOT/scripts/ssh-bridge/ssh" -G bridge-alias 2>&1)"; rc=$?
if [ $rc -eq 126 ] && echo "$out" | grep -q "缺少有效"; then ok "SSH bridge rejects missing authorization env"; else no "SSH bridge ran without authorization env (rc=$rc)"; fi

out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/arbitrary-linkhome" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9934 --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接"; then ok "launch rejects arbitrary symlinked data-dir"; else no "launch followed arbitrary symlinked data-dir (rc=$rc): $out"; fi

CAPTURE_FILE="$T/launch-args"
CAPTURE_ENV="$T/launch-env"
FAKE_CAPTURE="$T/fake-capture"
mkdir -p "$OUTER_HOME/.claude-science/runtime"
mkdir -p "$OUTER_HOME/.ssh"
printf 'must-not-copy\n' > "$OUTER_HOME/.claude-science/runtime/real-user-sentinel"
printf 'Host test-only second-alias test-only\n  HostName 127.0.0.1\n  Port 2222\n  User must-not-project\nHost *.wildcard !blocked\nHost=equals-alias\nhOsT lower-alias\nIdentityFile /must/not/project\n' > "$OUTER_HOME/.ssh/config"
printf '#!/bin/sh\nprintf "%%s\\n" "$@" > "$CAPTURE_FILE"\nif [ -n "${CAPTURE_ENV:-}" ]; then printf "PATH=%%s\\nSSH_CONFIG=%%s\\nHOME=%%s\\n" "$PATH" "${CSSWITCH_SYSTEM_SSH_CONFIG:-}" "$HOME" > "$CAPTURE_ENV"; fi\nexit 0\n' > "$FAKE_CAPTURE"
chmod +x "$FAKE_CAPTURE"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/vh-capture" SCIENCE_BIN="$FAKE_CAPTURE" CAPTURE_FILE="$CAPTURE_FILE" CAPTURE_ENV="$CAPTURE_ENV" CSSWITCH_REUSE_SYSTEM_SSH=1 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9940 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -eq 0 ] && grep -qx -- '--host' "$CAPTURE_FILE" && grep -qx -- '127.0.0.1' "$CAPTURE_FILE" && grep -qx -- '--sandbox-port' "$CAPTURE_FILE" && grep -qx -- '9941' "$CAPTURE_FILE"; then ok "launch pins loopback host and explicit Science preview port"; else no "launch omitted explicit loopback/preview port (rc=$rc): $out"; fi
if grep -Fq "PATH=$ROOT/scripts/ssh-bridge:" "$CAPTURE_ENV" && grep -Fxq "SSH_CONFIG=$OUTER_HOME/.ssh/config" "$CAPTURE_ENV" && grep -Fxq "HOME=$T/vh-capture" "$CAPTURE_ENV"; then ok "launch scopes SSH bridge to the isolated Science environment"; else no "launch omitted isolated SSH bridge environment"; fi
SANDBOX_SSH_CONFIG="$T/vh-capture/.ssh/config"
SANDBOX_SSH_MODE="$(stat -f '%Lp' "$SANDBOX_SSH_CONFIG" 2>/dev/null || true)"
if [ -f "$SANDBOX_SSH_CONFIG" ] && [ ! -L "$SANDBOX_SSH_CONFIG" ] && [ "$SANDBOX_SSH_MODE" = "600" ]; then ok "opt-in creates a narrow regular 0600 sandbox SSH config"; else no "opt-in did not create a safe sandbox SSH config"; fi
EXPECTED_ALIASES="$T/expected-host-aliases"
printf 'Host test-only\nHost second-alias\nHost equals-alias\nHost lower-alias\n' > "$EXPECTED_ALIASES"
ACTUAL_ALIASES="$T/actual-host-aliases"
grep '^Host ' "$SANDBOX_SSH_CONFIG" > "$ACTUAL_ALIASES" || true
if grep -Fxq '# CSSwitch managed system SSH config bridge v2' "$SANDBOX_SSH_CONFIG" && cmp -s "$EXPECTED_ALIASES" "$ACTUAL_ALIASES"; then ok "sandbox SSH config projects literal explicit Host aliases for Science"; else no "sandbox SSH config did not project the expected Host aliases"; fi
if ! grep -Eq 'HostName|Port|User|IdentityFile|must-not-project|must/not/project|wildcard|blocked' "$SANDBOX_SSH_CONFIG"; then ok "sandbox SSH alias projection excludes connection details and patterns"; else no "sandbox SSH alias projection copied non-alias config content"; fi
out_ssh="$(/usr/bin/ssh -F "$SANDBOX_SSH_CONFIG" -G test-only 2>/dev/null)"; rc_ssh=$?
if [ $rc_ssh -eq 0 ] && echo "$out_ssh" | grep -qx 'hostname 127.0.0.1' && echo "$out_ssh" | grep -qx 'port 2222'; then ok "sandbox SSH config Include resolves Host with system OpenSSH"; else no "sandbox SSH config did not resolve included Host (rc=$rc_ssh)"; fi
if [ ! -e "$T/vh-capture/.claude-science/runtime/real-user-sentinel" ]; then ok "launch never copies real Science runtime data"; else no "launch copied real Science data into sandbox"; fi
if ! echo "$out" | grep -Fq "$T/vh-capture" && ! echo "$out" | grep -Fq "$FAKE_CAPTURE"; then ok "launch log redacts sandbox and binary paths"; else no "launch log exposed sensitive paths: $out"; fi

out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/vh-capture" SCIENCE_BIN="$FAKE_CAPTURE" CAPTURE_FILE="$CAPTURE_FILE" CAPTURE_ENV="$CAPTURE_ENV" CSSWITCH_REUSE_SYSTEM_SSH=0 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9940 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -eq 0 ] && [ ! -e "$SANDBOX_SSH_CONFIG" ] && [ ! -L "$SANDBOX_SSH_CONFIG" ]; then ok "disabling SSH removes only the managed sandbox config"; else no "disabling SSH left the managed sandbox config or failed (rc=$rc): $out"; fi

V1_SANDBOX="$T/vh-v1-ssh"
mkdir -p "$V1_SANDBOX/.ssh"
printf '# CSSwitch managed system SSH config bridge v1\nInclude "%s"\n' "$OUTER_HOME/.ssh/config" > "$V1_SANDBOX/.ssh/config"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$V1_SANDBOX" SCIENCE_BIN="$FAKE_CAPTURE" CSSWITCH_REUSE_SYSTEM_SSH=1 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9945 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -eq 0 ] && grep -Fxq '# CSSwitch managed system SSH config bridge v2' "$V1_SANDBOX/.ssh/config" && grep -Fxq 'Host test-only' "$V1_SANDBOX/.ssh/config"; then ok "SSH opt-in atomically migrates the managed v1 stub to v2 aliases"; else no "SSH opt-in did not migrate the managed v1 stub (rc=$rc): $out"; fi

V1_REVOKE_SANDBOX="$T/vh-v1-revoke-ssh"
mkdir -p "$V1_REVOKE_SANDBOX/.ssh"
printf '# CSSwitch managed system SSH config bridge v1\nInclude "%s"\n' "$OUTER_HOME/.ssh/config" > "$V1_REVOKE_SANDBOX/.ssh/config"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$V1_REVOKE_SANDBOX" SCIENCE_BIN="$FAKE_CAPTURE" CSSWITCH_REUSE_SYSTEM_SSH=0 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9946 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -eq 0 ] && [ ! -e "$V1_REVOKE_SANDBOX/.ssh/config" ]; then ok "disabling SSH still revokes a managed v1 stub"; else no "disabling SSH did not revoke the managed v1 stub (rc=$rc): $out"; fi

FOREIGN_SANDBOX="$T/vh-foreign-ssh"
mkdir -p "$FOREIGN_SANDBOX/.ssh"
printf 'Host foreign\n' > "$FOREIGN_SANDBOX/.ssh/config"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$FOREIGN_SANDBOX" SCIENCE_BIN="$FAKE_CAPTURE" CSSWITCH_REUSE_SYSTEM_SSH=1 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9942 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "不是 CSSwitch 管理" && grep -qx 'Host foreign' "$FOREIGN_SANDBOX/.ssh/config"; then ok "SSH opt-in refuses and preserves a foreign sandbox config"; else no "SSH opt-in overwrote or accepted a foreign sandbox config (rc=$rc): $out"; fi

FORGED_SANDBOX="$T/vh-forged-ssh"
mkdir -p "$FORGED_SANDBOX/.ssh"
printf '# CSSwitch managed system SSH config bridge v1\nInclude "/different/config"\n\nHost must-survive\n' > "$FORGED_SANDBOX/.ssh/config"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$FORGED_SANDBOX" SCIENCE_BIN="$FAKE_CAPTURE" CSSWITCH_REUSE_SYSTEM_SSH=1 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9943 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && grep -q 'Host must-survive' "$FORGED_SANDBOX/.ssh/config"; then ok "SSH opt-in preserves a forged marker with a different Include"; else no "SSH opt-in trusted or overwrote a forged marker (rc=$rc): $out"; fi

FORGED_V2_SANDBOX="$T/vh-forged-v2-ssh"
mkdir -p "$FORGED_V2_SANDBOX/.ssh"
printf '# CSSwitch managed system SSH config bridge v2\nInclude "%s"\nHost plausible\nUser must-survive\n' "$OUTER_HOME/.ssh/config" > "$FORGED_V2_SANDBOX/.ssh/config"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$FORGED_V2_SANDBOX" SCIENCE_BIN="$FAKE_CAPTURE" CSSWITCH_REUSE_SYSTEM_SSH=1 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9947 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && grep -q 'User must-survive' "$FORGED_V2_SANDBOX/.ssh/config"; then ok "SSH opt-in preserves a malformed v2 managed-looking config"; else no "SSH opt-in trusted or overwrote malformed v2 content (rc=$rc): $out"; fi

NO_NEWLINE_V2_SANDBOX="$T/vh-no-newline-v2-ssh"
mkdir -p "$NO_NEWLINE_V2_SANDBOX/.ssh"
printf '# CSSwitch managed system SSH config bridge v2\nInclude "%s"\nHost must-survive' "$OUTER_HOME/.ssh/config" > "$NO_NEWLINE_V2_SANDBOX/.ssh/config"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$NO_NEWLINE_V2_SANDBOX" SCIENCE_BIN="$FAKE_CAPTURE" CSSWITCH_REUSE_SYSTEM_SSH=1 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9948 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && grep -q 'Host must-survive' "$NO_NEWLINE_V2_SANDBOX/.ssh/config"; then ok "SSH opt-in preserves a noncanonical v2 config without final newline"; else no "SSH opt-in trusted or overwrote a noncanonical v2 config (rc=$rc): $out"; fi

OVERFLOW_HOME="$T/overflow-home"
OVERFLOW_SANDBOX="$T/vh-overflow-ssh"
mkdir -p "$OVERFLOW_HOME/.ssh"
for index in $(seq 0 512); do printf 'Host overflow-%s\n' "$index"; done > "$OVERFLOW_HOME/.ssh/config"
out="$(HOME="$OVERFLOW_HOME" SANDBOX_HOME="$OVERFLOW_SANDBOX" SCIENCE_BIN="$FAKE_CAPTURE" CSSWITCH_REUSE_SYSTEM_SSH=1 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9949 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q '无法安全提取系统 SSH Host alias' && [ ! -e "$OVERFLOW_SANDBOX/.ssh/config" ]; then ok "SSH alias projection fails closed above the alias limit"; else no "SSH alias projection accepted or hid an oversized alias set (rc=$rc): $out"; fi

LINK_SANDBOX="$T/vh-linked-ssh"
mkdir -p "$LINK_SANDBOX" "$T/ssh-link-target"
ln -s "$T/ssh-link-target" "$LINK_SANDBOX/.ssh"
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$LINK_SANDBOX" SCIENCE_BIN="$FAKE_CAPTURE" CSSWITCH_REUSE_SYSTEM_SSH=1 "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9944 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接" && [ ! -e "$T/ssh-link-target/config" ]; then ok "SSH opt-in rejects a symlinked sandbox .ssh directory"; else no "SSH opt-in followed a symlinked sandbox .ssh directory (rc=$rc): $out"; fi

python3 -c 'import socket,time; s=socket.socket(); s.bind(("127.0.0.1",29992)); s.listen(); time.sleep(10)' &
PREVIEW_HOLDER=$!
sleep 0.2
out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/vh-preview-conflict" SCIENCE_BIN="$FAKE_CAPTURE" CAPTURE_FILE="$CAPTURE_FILE" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 29991 --skip-oauth-forge 2>&1)"; rc=$?
kill "$PREVIEW_HOLDER" 2>/dev/null || true
wait "$PREVIEW_HOLDER" 2>/dev/null || true
if [ $rc -ne 0 ] && echo "$out" | grep -q "预览端口.*占用"; then ok "launch rejects occupied preview listener without takeover"; else no "launch ignored occupied preview listener (rc=$rc): $out"; fi

out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/vh-link" SCIENCE_BIN="$FAKE_LINK" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9932 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接"; then ok "launch rejects explicit Science symlink"; else no "launch accepted explicit Science symlink (rc=$rc): $out"; fi

out="$(HOME="$OUTER_HOME" SANDBOX_HOME="$T/vh-parent-link" SCIENCE_BIN="$PARENT_LINK_BIN" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 9933 --skip-oauth-forge 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "符号链接"; then ok "launch rejects symlinked Science parent"; else no "launch accepted symlinked Science parent (rc=$rc): $out"; fi

# 7.7 review: 畸形端口必须失败关闭（fail-closed），而不是绕过算术守卫
out="$(SANDBOX_HOME="$T/vh2" "$ROOT/scripts/launch-virtual-sandbox.sh" --port 8765x --dry-run 2>&1)"; rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -q "拒绝"; then ok "malformed port 8765x rejected fail-closed"; else no "malformed port 8765x slipped guard (rc=$rc): $out"; fi

echo "----"
if [ $FAILS -eq 0 ]; then echo "ALL PASS"; exit 0; else echo "$FAILS FAILED"; exit 1; fi
