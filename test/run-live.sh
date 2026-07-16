#!/usr/bin/env bash
# Real-machine dynamic track placeholder.
# This runner is intentionally not wired into S0 because it needs real local state.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"

echo "live track needs a real machine with Claude Science, allowed loopback, and explicitly authorized provider credentials or Codex OAuth."
echo "For Codex, prepare the isolated Acceptance environment first and stop before opening OAuth until the user is present."
echo "See docs/operations/real-machine-acceptance.md for the current manual checklist."
echo "CS_TEST_LAYER live needs-real-machine"
exit 0
