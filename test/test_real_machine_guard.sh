#!/usr/bin/env bash
# Offline contract checks for the real-machine guard.  These checks never launch
# Science, OAuth, the CSSwitch runtime, or any Keychain operation.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GUARD="$ROOT/test/real_machine_guard.sh"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/csswitch-guard-test.XXXXXX")"
REAL_HOME="$TMP_ROOT/real-home"
ACCEPTANCE_ROOT="$TMP_ROOT/acceptance"
FAILS=0

cleanup() { rm -rf "$TMP_ROOT"; }
trap cleanup EXIT

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; FAILS=$((FAILS + 1)); }

mkdir -p "$REAL_HOME/.csswitch"
printf '%s\n' 'real-config-sentinel' >"$REAL_HOME/.csswitch/config.json"
REAL_BEFORE="$(shasum -a 256 "$REAL_HOME/.csswitch/config.json" | awk '{print $1}')"
mkdir -p "$REAL_HOME/.csswitch-acceptance"
printf '%s\n' 'real-acceptance-sentinel' >"$REAL_HOME/.csswitch-acceptance/config.json"
REAL_ACCEPTANCE_BEFORE="$(shasum -a 256 "$REAL_HOME/.csswitch-acceptance/config.json" | awk '{print $1}')"

guard() {
  HOME="$REAL_HOME" \
  CSSWITCH_REAL_TEST_ROOT="$ACCEPTANCE_ROOT" \
  SCIENCE_BIN=/usr/bin/true \
    bash "$GUARD" "$@"
}

if guard preflight >/dev/null; then
  pass "preflight creates an isolated dynamic-port environment"
else
  fail "preflight failed"
fi

if [ ! -e "$ACCEPTANCE_ROOT/home/Library/Keychains" ]; then
  pass "preflight performs no Keychain setup"
else
  fail "preflight unexpectedly created a Keychain directory"
fi

ENV_OUT="$(guard env 2>/dev/null || true)"
PROXY_PORT="$(printf '%s\n' "$ENV_OUT" | awk -F= '$1 == "CSSWITCH_TEST_PROXY_PORT" { print $2 }')"
SANDBOX_PORT="$(printf '%s\n' "$ENV_OUT" | awk -F= '$1 == "CSSWITCH_TEST_SANDBOX_PORT" { print $2 }')"

case "$PROXY_PORT:$SANDBOX_PORT" in
  *[!0-9:]*|:*) fail "generated ports are not numeric" ;;
  *)
    if [ "$PROXY_PORT" != "$SANDBOX_PORT" ] && \
       [ "$PROXY_PORT" != 8765 ] && [ "$SANDBOX_PORT" != 8765 ] && \
       [ "$PROXY_PORT" != 1455 ] && [ "$PROXY_PORT" != 1457 ] && \
       [ "$SANDBOX_PORT" != 1455 ] && [ "$SANDBOX_PORT" != 1457 ]; then
      pass "dynamic ports are distinct and avoid reserved ports"
    else
      fail "dynamic ports collide with each other or a reserved port"
    fi
    ;;
esac

if [ "$(stat -f '%Lp' "$ACCEPTANCE_ROOT/state/runtime-ports.v1")" = 600 ]; then
  pass "persisted port state is mode 0600"
else
  fail "persisted port state is not mode 0600"
fi

if guard prepare-codex >/dev/null; then
  pass "prepare-codex creates an isolated v3 fixture"
else
  fail "prepare-codex failed"
fi

CFG="$ACCEPTANCE_ROOT/home/.csswitch-acceptance/config.json"
if jq -e \
  --argjson proxy "$PROXY_PORT" \
  --argjson sandbox "$SANDBOX_PORT" \
  '(.schema_version == 3)
   and (.profiles == [])
   and (.active_id == "")
   and (.proxy_port == $proxy)
   and (.sandbox_port == $sandbox)
   and (.experimental_codex_enabled == false)
   and ([.. | objects | keys[]] | index("token") == null)
   and ([.. | objects | keys[]] | index("credential_ref") == null)' \
  "$CFG" >/dev/null; then
  pass "Codex fixture is default-off and contains no credential material"
else
  fail "Codex fixture contract mismatch"
fi

if [ "$(stat -f '%Lp' "$CFG")" = 600 ] && \
   [ "$(stat -f '%Lp' "$ACCEPTANCE_ROOT/home/.csswitch-acceptance")" = 700 ]; then
  pass "Codex fixture permissions are 0600/0700"
else
  fail "Codex fixture permissions are too broad"
fi

if guard prepare-codex >/dev/null 2>&1; then
  fail "prepare-codex overwrote an existing acceptance config"
else
  pass "prepare-codex refuses to overwrite acceptance state"
fi

if guard assert-stopped >/dev/null; then
  pass "fresh acceptance ports are stopped and 8765 baseline is unchanged"
else
  fail "stopped-state guard failed"
fi

REAL_AFTER="$(shasum -a 256 "$REAL_HOME/.csswitch/config.json" | awk '{print $1}')"
if [ "$REAL_BEFORE" = "$REAL_AFTER" ]; then
  pass "real-HOME sentinel was not modified"
else
  fail "real-HOME sentinel changed"
fi

REAL_ACCEPTANCE_AFTER="$(shasum -a 256 "$REAL_HOME/.csswitch-acceptance/config.json" | awk '{print $1}')"
if [ "$REAL_ACCEPTANCE_BEFORE" = "$REAL_ACCEPTANCE_AFTER" ]; then
  pass "real-HOME Acceptance sentinel was not modified"
else
  fail "real-HOME Acceptance sentinel changed"
fi

if grep -q -- '--features acceptance-build' \
     "$ROOT/docs/operations/real-machine-acceptance.md" && \
   grep -q '^acceptance-build = \[\]$' "$ROOT/desktop/src-tauri/Cargo.toml" && \
   grep -q '^acceptance-build = \[\]$' "$ROOT/desktop/gateway/Cargo.toml" && \
   grep -q 'CARGO_FEATURE_ACCEPTANCE_BUILD' "$ROOT/desktop/src-tauri/build.rs" && \
   ! grep -q 'CSSWITCH_SIGNING_TEAM_ID' "$ROOT/desktop/src-tauri/build.rs" && \
   [ ! -e "$ROOT/desktop/src-tauri/src/code_identity.rs" ] && \
   [ ! -e "$ROOT/desktop/gateway/src/code_identity.rs" ] && \
   [ ! -e "$ROOT/scripts/sign-csswitch-macos.sh" ] && \
   ! grep -q 'signingIdentity.*"-"' "$ROOT/desktop/src-tauri/tauri.conf.json" && \
   grep -q 'CSSwitch builds cannot skip Gateway staging' \
     "$ROOT/desktop/src-tauri/build.rs" && \
   ! grep -q 'CSSWITCH_EXPECTED_CODEX_KEYCHAIN_SERVICE' \
     "$ROOT/desktop/gateway/src/main.rs" && \
   grep -q 'OAUTH_SECRET_FILE.*codex-oauth.v1.json' \
     "$ROOT/desktop/gateway/src/codex_auth/storage.rs" && \
   grep -q 'CONFIG_DIR_NAME: &str = ".csswitch-acceptance"' \
     "$ROOT/desktop/src-tauri/src/config.rs" && \
   grep -q 'CODEX_STATE_DIR_NAME: &str = ".csswitch-acceptance"' \
     "$ROOT/desktop/gateway/src/codex_auth/mod.rs"; then
  pass "Acceptance build is pinned to an isolated data root and needs no signing or Keychain"
else
  fail "Acceptance no-signing file-storage isolation contract is incomplete"
fi

if HOME="$REAL_HOME" \
   CSSWITCH_REAL_TEST_ROOT="$TMP_ROOT/reserved-port" \
   CSSWITCH_TEST_PROXY_PORT=1455 \
   CSSWITCH_TEST_SANDBOX_PORT=34999 \
   SCIENCE_BIN=/usr/bin/true \
     bash "$GUARD" preflight >/dev/null 2>&1; then
  fail "preflight accepted a reserved OAuth callback port"
else
  pass "preflight rejects runtime use of OAuth callback ports"
fi

if env PATH=/usr/bin:/bin \
   HOME="$REAL_HOME" \
   CSSWITCH_REAL_TEST_ROOT="$TMP_ROOT/no-lsof" \
   SCIENCE_BIN=/usr/bin/true \
     /bin/bash "$GUARD" preflight >/dev/null 2>&1; then
  fail "preflight treated missing lsof as an empty listener set"
else
  pass "preflight fails closed when lsof is unavailable"
fi

FAKE_BIN="$TMP_ROOT/fake-bin"
mkdir -p "$FAKE_BIN"
cat >"$FAKE_BIN/lsof" <<'FAKE_LSOF'
#!/usr/bin/env bash
if [ "${1:-}" = "-v" ]; then
  exit 0
fi
if [ "${FAKE_LSOF_MODE:-}" = "callback-error" ]; then
  case " $* " in
    *" -iTCP:1455 "*) exit 2 ;;
    *) exit 1 ;;
  esac
fi
case " $* " in
  *" -iTCP:8765 "*) exit 1 ;;
  *) exit 2 ;;
esac
FAKE_LSOF
chmod 700 "$FAKE_BIN/lsof"
if env PATH="$FAKE_BIN:/usr/bin:/bin:/usr/sbin" \
   HOME="$REAL_HOME" \
   CSSWITCH_REAL_TEST_ROOT="$TMP_ROOT/lsof-query-error" \
   SCIENCE_BIN=/usr/bin/true \
     /bin/bash "$GUARD" preflight >/dev/null 2>&1; then
  fail "preflight swallowed a per-port lsof query failure"
else
  pass "preflight fails closed on a per-port lsof query error"
fi

if env PATH="$FAKE_BIN:/usr/bin:/bin:/usr/sbin" \
   FAKE_LSOF_MODE=callback-error \
   HOME="$REAL_HOME" \
   CSSWITCH_REAL_TEST_ROOT="$TMP_ROOT/lsof-callback-error" \
   SCIENCE_BIN=/usr/bin/true \
     /bin/bash "$GUARD" preflight >/dev/null 2>&1; then
  fail "preflight swallowed an OAuth callback lsof query failure"
else
  pass "preflight fails closed on an OAuth callback query error"
fi

mkdir -p "$TMP_ROOT/symlink-target"
ln -s "$TMP_ROOT/symlink-target" "$TMP_ROOT/symlink-root"
if HOME="$REAL_HOME" \
   CSSWITCH_REAL_TEST_ROOT="$TMP_ROOT/symlink-root" \
   SCIENCE_BIN=/usr/bin/true \
     bash "$GUARD" preflight >/dev/null 2>&1; then
  fail "preflight accepted a symlinked isolation root"
else
  pass "preflight rejects a symlinked isolation root"
fi

if [ "$FAILS" -eq 0 ]; then
  echo "REAL_MACHINE_GUARD_TESTS pass"
  exit 0
fi
echo "REAL_MACHINE_GUARD_TESTS fail=$FAILS" >&2
exit 1
