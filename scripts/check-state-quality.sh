#!/usr/bin/env bash
# Repeatable engine state-quality evaluation. No provider calls in the default mode.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  echo "Usage: scripts/check-state-quality.sh [--live codex|claude-code|gemini-cli]"
  echo "Default: deterministic ingest/review/consolidation/recall and grounding regressions."
  echo "--live: six synthetic-text model steps on the selected CLI's signed-in account."
}

case "${1:-}" in
  --help|-h) usage; exit 0 ;;
  "")
    if [ "$#" -ne 0 ]; then usage >&2; exit 2; fi
    exec cargo test --locked -p shogun-core --features daemon-server --test state_quality_e2e -- --nocapture
    ;;
  --live)
    if [ "$#" -ne 2 ]; then usage >&2; exit 2; fi
    case "$2" in
      codex|claude-code|gemini-cli) ;;
      *) usage >&2; exit 2 ;;
    esac
    export SHOGUN_STATE_QUALITY_LIVE="$2"
    exec cargo test --locked -p shogun-core --features daemon-server --test state_quality_e2e live_state_quality -- --ignored --exact --nocapture --test-threads=1
    ;;
  *) usage >&2; exit 2 ;;
esac
