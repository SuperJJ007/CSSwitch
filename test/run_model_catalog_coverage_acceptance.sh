#!/usr/bin/env bash
# Build HEAD's schema-v3 Acceptance bundle and the current dirty-tree
# Acceptance bundle, replace the same isolated installation path, then execute
# the local-mock model-catalog Acceptance. Nothing is installed to /Applications.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
RUN_ROOT="${CSSWITCH_MODEL_CATALOG_ACCEPTANCE_ROOT:-/private/tmp/csmc-${STAMP}}"

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
for required in "$CARGO_BIN" "$NODE_BIN" "$NPM_BIN" "$PYTHON_BIN" /usr/bin/ditto /usr/bin/tar; do
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
mkdir -m 700 "$OLD_SOURCE" "$ARTIFACTS"

git -C "$ROOT" archive --format=tar HEAD | /usr/bin/tar -x -C "$OLD_SOURCE"
ln -s "$ROOT/desktop/node_modules" "$OLD_SOURCE/desktop/node_modules"
/usr/bin/ditto "$ROOT/test/tauri.model-catalog-acceptance.conf.json" \
  "$OLD_SOURCE/test/tauri.model-catalog-acceptance.conf.json"

export PATH="$(dirname "$CARGO_BIN"):$(dirname "$NODE_BIN"):$(dirname "$NPM_BIN"):/usr/bin:/bin:/usr/sbin:/sbin"
unset CSSWITCH_SKIP_GATEWAY_STAGE

sign_and_verify() {
  local bundle="$1"
  /usr/bin/xattr -cr "$bundle"
  /usr/bin/codesign --force --deep --sign "$SIGN_IDENTITY" "$bundle"
  /usr/bin/codesign --verify --deep --strict "$bundle"
}

echo "[1/3] Building old schema-v3 Acceptance bundle from $(git -C "$ROOT" rev-parse HEAD)"
(
  cd "$OLD_SOURCE/desktop"
  "$NPM_BIN" run tauri build -- \
    --features acceptance-build \
    --config ../test/tauri.model-catalog-acceptance.conf.json \
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
    --config ../test/tauri.model-catalog-acceptance.conf.json \
    --bundles app
)
NEW_BUILT="$ROOT/desktop/src-tauri/target/release/bundle/macos/CSSwitch Model Catalog Acceptance.app"
NEW_ARTIFACT="$ARTIFACTS/new/CSSwitch Model Catalog Acceptance.app"
mkdir -m 700 "$ARTIFACTS/new"
/usr/bin/ditto --rsrc "$NEW_BUILT" "$NEW_ARTIFACT"
sign_and_verify "$NEW_ARTIFACT"

echo "[3/3] Replacing the isolated installation and running v3 -> v4 Acceptance"
PYTHONDONTWRITEBYTECODE=1 "$PYTHON_BIN" "$ROOT/test/model_catalog_coverage_acceptance.py" \
  --old-bundle "$OLD_ARTIFACT" \
  --new-bundle "$NEW_ARTIFACT" \
  --root "$RUN_ROOT/coverage-run" \
  | tee "$RUN_ROOT/coverage-result.json"

echo "PASS: evidence preserved at $RUN_ROOT"
