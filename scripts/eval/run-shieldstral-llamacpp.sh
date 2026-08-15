#!/bin/sh
# Measurement 1 for the Shieldstral guard-tier study: bring up llama.cpp with
# Shieldstral and run the logprobs probe against it.
#
# Why llama.cpp and not the default backend: oMLX (the macOS default since
# 2026-08-15) returns no token logprobs, so the calibrated score the whole
# confidence-band design rests on is unavailable there. See
# docs/superpowers/specs/2026-08-13-shieldstral-guard-model-feasibility-study.md
#
# Usage:
#   MODEL=~/models/shieldstral/Shieldstral-1.0-3B-Q4_K_M.gguf \
#   MMPROJ=~/models/shieldstral/mmproj-F16.gguf \
#   sh scripts/eval/run-shieldstral-llamacpp.sh
set -eu

MODEL="${MODEL:-$HOME/models/shieldstral/Shieldstral-1.0-3B-Q4_K_M.gguf}"
MMPROJ="${MMPROJ:-$HOME/models/shieldstral/mmproj-F16.gguf}"
PORT="${PORT:-8081}"
LOG="${LOG:-$HOME/llama-shieldstral.log}"

[ -f "$MODEL" ] || { echo "missing model: $MODEL"; exit 2; }

MM_ARGS=""
[ -f "$MMPROJ" ] && MM_ARGS="--mmproj $MMPROJ"

echo "starting llama-server on :$PORT"
# --alias so the API model name is stable; jinja is on by default so the
# GGUF's own chat template is applied -- which is the thing to verify below,
# since an upstream GGUF shipping a broken stub template is a known hazard
# (it bit the Agents-A1 import) and would silently corrupt every score.
llama-server -m "$MODEL" $MM_ARGS \
  --alias shieldstral --port "$PORT" --host 127.0.0.1 \
  -c 8192 -ngl 99 --no-webui > "$LOG" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT INT TERM

# Wait for readiness rather than sleeping a guessed interval.
#
# Must require HTTP **200**, not merely a reachable socket: llama.cpp answers
# /health with 503 while the model is still loading, and a bare `curl -s ...
# >/dev/null` treats that as success -- which sends the probe in early and
# surfaces as a bogus "cannot reach backend" failure of the measurement.
ready=""
for _ in $(seq 1 300); do
  code=$(curl -s -m 2 -o /dev/null -w '%{http_code}' \
         "http://127.0.0.1:$PORT/health" 2>/dev/null || echo 000)
  [ "$code" = "200" ] && { ready=1; break; }
  kill -0 $SRV 2>/dev/null || { echo "server died:"; tail -20 "$LOG"; exit 3; }
  sleep 1
done
[ -n "$ready" ] || { echo "server never became ready:"; tail -20 "$LOG"; exit 4; }

echo
echo "=== chat template preflight (a broken stub template silently ruins the score) ==="
grep -iE "chat template|chat_template|tmpl" "$LOG" | head -5 || echo "  (nothing logged)"

echo
MM_FLAG=""
[ -n "$MM_ARGS" ] && MM_FLAG="--multimodal"
python3 "$(dirname "$0")/shieldstral_logprobs_probe.py" \
  --base-url "http://127.0.0.1:$PORT/v1/chat/completions" \
  --model shieldstral $MM_FLAG
