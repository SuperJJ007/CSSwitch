#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

for dir in "$ROOT/proxy" "$ROOT/scripts"; do
  [ -d "$dir" ] || continue
  find "$dir" -type d -name '__pycache__' -prune -exec rm -rf {} +
  find "$dir" -type f -name '*.pyc' -delete
  find "$dir" -type f -name '*.pyo' -delete
done
