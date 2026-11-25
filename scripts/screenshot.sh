#!/usr/bin/env bash
# Captures a screenshot of the standalone app's window.
#
#   scripts/screenshot.sh out.png [demo-name] [wait-seconds]
#
# demo-name selects a library entry on launch (part of its name).
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="$1"; DEMO="${2:-}"; WAIT="${3:-8}"
cargo build --release --bin standalone >/dev/null 2>&1
TEMPO_SAMPLER_DEMO="$DEMO" TEMPO_SAMPLER_MODEL="$PWD/models/htdemucs_fp16weights.onnx" \
  ./target/release/standalone --backend dummy >/dev/null 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT
osascript -e "delay $WAIT" >/dev/null
WID=""
for _ in 1 2 3 4 5; do
  WID=$(swift scripts/window_id.swift "$PID" 2>/dev/null || true)
  [[ -n "$WID" ]] && break
  osascript -e "delay 1" >/dev/null
done
[[ -n "$WID" ]] || { echo "window not found" >&2; exit 1; }
screencapture -l "$WID" -o "$OUT"
echo "$OUT"
