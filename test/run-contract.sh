#!/usr/bin/env bash
# CS Native-style contract layer.
# This is a working overlay for refactor tasks; it does not change S0 release gate semantics.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"

fail=0
blocked=0

run_s0_layer() {
  layer="$1"
  out="$(bash "test/run-$layer.sh" 2>&1)"
  rc=$?
  printf '%s\n' "$out"
  line="$(printf '%s\n' "$out" | grep -E '^S0_LAYER ' | tail -1)"
  st="$(printf '%s\n' "$line" | awk '{print $3}')"
  if [ "$rc" -ne 0 ] || [ -z "$st" ]; then
    fail=1
    return
  fi
  case "$st" in
    pass) : ;;
    fail) fail=1 ;;
    *) blocked=1 ;;
  esac
}

run_s0_layer offline
run_s0_layer rust

if [ "$fail" -ne 0 ]; then
  echo "CS_TEST_LAYER contract fail"; exit 1
fi
if [ "$blocked" -ne 0 ]; then
  echo "CS_TEST_LAYER contract env-blocked"; exit 0
fi
echo "CS_TEST_LAYER contract pass"
exit 0
