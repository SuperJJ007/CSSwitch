#!/usr/bin/env bash
# Build the last schema-v3 Acceptance source and the current dirty-tree
# Acceptance bundle, replace the same isolated installation path, then execute
# the local-mock model-catalog Acceptance. Nothing is installed to /Applications.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
RUN_ROOT="${CSSWITCH_MODEL_CATALOG_ACCEPTANCE_ROOT:-/private/tmp/csmc-${STAMP}}"
V3_REF="${CSSWITCH_MODEL_CATALOG_V3_REF:-bd1f3ab836c05ba40854e128458f8e98917eb230}"
ACCEPTANCE_BUNDLE_ID="com.csswitch.acceptance.modelcatalog.coverage.run$(printf '%s' "$STAMP" | tr -d '-')"

case "$RUN_ROOT" in
  /private/tmp/* | /tmp/*) ;;
  *) echo "FAIL: acceptance root must be under /private/tmp or /tmp" >&2; exit 1 ;;
esac
if [ -e "$RUN_ROOT" ] || [ -L "$RUN_ROOT" ]; then
  echo "FAIL: acceptance root already exists: $RUN_ROOT" >&2
  exit 1
fi

CARGO_BIN="$(command -v cargo 2>/dev/null || true)"
if [ -z "$CARGO_BIN" ] && [ -x "$HOME/.cargo/bin/cargo" ]; then
  CARGO_BIN="$HOME/.cargo/bin/cargo"
fi
NODE_BIN="$(command -v node 2>/dev/null || true)"
NPM_BIN="$(command -v npm 2>/dev/null || true)"
PYTHON_BIN="${CSSWITCH_ACCEPTANCE_PYTHON:-}"
SIGN_IDENTITY="${CSSWITCH_ACCEPTANCE_SIGN_IDENTITY:--}"
if [ -z "$PYTHON_BIN" ]; then
  for candidate in /usr/local/bin/python3 /opt/homebrew/bin/python3 "$(command -v python3 2>/dev/null || true)"; do
    if [ -x "$candidate" ] && "$candidate" -c 'import sys; raise SystemExit(sys.version_info < (3, 10))'; then
      PYTHON_BIN="$candidate"
      break
    fi
  done
fi
for required in "$CARGO_BIN" "$NODE_BIN" "$NPM_BIN" "$PYTHON_BIN" /usr/bin/clang /usr/bin/ditto /usr/bin/tar; do
  if [ -z "$required" ] || [ ! -x "$required" ]; then
    echo "FAIL: required build tool is unavailable: ${required:-unset}" >&2
    exit 1
  fi
done
if [ ! -x "$ROOT/desktop/node_modules/.bin/tauri" ]; then
  echo "FAIL: current workspace node_modules/@tauri-apps/cli is required" >&2
  exit 1
fi

mkdir -m 700 "$RUN_ROOT"
OLD_SOURCE="$RUN_ROOT/old-source"
ARTIFACTS="$RUN_ROOT/artifacts"
BUILD_CONFIG="$RUN_ROOT/tauri.model-catalog-coverage.conf.json"
FAKE_SCIENCE_BIN="$RUN_ROOT/fake-science-runtime"
mkdir -m 700 "$OLD_SOURCE" "$ARTIFACTS"

/usr/bin/clang -std=c11 -O2 -Wall -Wextra -Werror \
  "$ROOT/test/fake_science_runtime.c" -o "$FAKE_SCIENCE_BIN"
chmod 700 "$FAKE_SCIENCE_BIN"

V3_COMMIT="$(git -C "$ROOT" rev-parse --verify "${V3_REF}^{commit}")" || {
  echo "FAIL: schema-v3 source ref is unavailable: $V3_REF" >&2
  exit 1
}
if ! git -C "$ROOT" show "$V3_COMMIT:desktop/src-tauri/src/config.rs" \
  | /usr/bin/grep -q 'CURRENT_SCHEMA_VERSION: u32 = 3;'; then
  echo "FAIL: configured old source is not schema v3: $V3_COMMIT" >&2
  exit 1
fi

git -C "$ROOT" archive --format=tar "$V3_COMMIT" | /usr/bin/tar -x -C "$OLD_SOURCE"
ln -s "$ROOT/desktop/node_modules" "$OLD_SOURCE/desktop/node_modules"
/usr/bin/sed \
  "s/com\.csswitch\.acceptance\.modelcatalog/$ACCEPTANCE_BUNDLE_ID/" \
  "$ROOT/test/tauri.model-catalog-acceptance.conf.json" > "$BUILD_CONFIG"

export PATH="$(dirname "$CARGO_BIN"):$(dirname "$NODE_BIN"):$(dirname "$NPM_BIN"):/usr/bin:/bin:/usr/sbin:/sbin"
unset CSSWITCH_SKIP_GATEWAY_STAGE

sign_and_verify() {
  local bundle="$1"
  /usr/bin/xattr -cr "$bundle"
  /usr/bin/codesign --force --deep --sign "$SIGN_IDENTITY" "$bundle"
  /usr/bin/codesign --verify --deep --strict "$bundle"
}

echo "[1/3] Building old schema-v3 Acceptance bundle from $V3_COMMIT"
(
  cd "$OLD_SOURCE/desktop"
  "$NPM_BIN" run tauri build -- \
    --features acceptance-build \
    --config "$BUILD_CONFIG" \
    --bundles app
)
OLD_BUILT="$OLD_SOURCE/desktop/src-tauri/target/release/bundle/macos/CSSwitch Model Catalog Acceptance.app"
OLD_ARTIFACT="$ARTIFACTS/old/CSSwitch Model Catalog Acceptance.app"
mkdir -m 700 "$ARTIFACTS/old"
/usr/bin/ditto --rsrc "$OLD_BUILT" "$OLD_ARTIFACT"
sign_and_verify "$OLD_ARTIFACT"

echo "[2/3] Building current dirty-tree Acceptance bundle"
(
  cd "$ROOT/desktop"
  "$NPM_BIN" run tauri build -- \
    --features acceptance-build \
    --config "$BUILD_CONFIG" \
    --bundles app
)
NEW_BUILT="$ROOT/desktop/src-tauri/target/release/bundle/macos/CSSwitch Model Catalog Acceptance.app"
NEW_ARTIFACT="$ARTIFACTS/new/CSSwitch Model Catalog Acceptance.app"
mkdir -m 700 "$ARTIFACTS/new"
/usr/bin/ditto --rsrc "$NEW_BUILT" "$NEW_ARTIFACT"
sign_and_verify "$NEW_ARTIFACT"

echo "[3/3] Replacing the isolated installation and running v3 -> v4 Acceptance"
CSSWITCH_ACCEPTANCE_FAKE_SCIENCE_BIN="$FAKE_SCIENCE_BIN" \
PYTHONDONTWRITEBYTECODE=1 "$PYTHON_BIN" "$ROOT/test/model_catalog_coverage_acceptance.py" \
  --old-bundle "$OLD_ARTIFACT" \
  --new-bundle "$NEW_ARTIFACT" \
  --expected-bundle-id "$ACCEPTANCE_BUNDLE_ID" \
  --v3-commit "$V3_COMMIT" \
  --root "$RUN_ROOT/coverage-run" \
  | tee "$RUN_ROOT/coverage-result.json"

echo "PASS: evidence preserved at $RUN_ROOT"
