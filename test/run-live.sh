#!/usr/bin/env bash
# Real-machine dynamic track placeholder.
# This runner is intentionally not wired into S0 because it needs real local state.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"

echo "live track needs a real machine with Claude Science, allowed loopback, and chosen provider credentials."
echo "See docs/operations/real-machine-acceptance.md for the current manual checklist."
echo "CS_TEST_LAYER live needs-real-machine"
exit 0
