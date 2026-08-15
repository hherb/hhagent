#!/bin/sh
# Measurement 1 for the Shieldstral guard-tier study: bring up llama.cpp with
# Shieldstral and run the logprobs probe against it.
#
# Why llama.cpp and not the default backend: oMLX (the macOS default) returns
# no token logprobs, so the calibrated score the whole confidence-band design
# rests on is unavailable there. See
# docs/superpowers/specs/2026-08-13-shieldstral-guard-model-feasibility-study.md
#
# The exit status is the MEASUREMENT's verdict, not this script's liveness:
# it is the probe's, and the probe fails non-zero on any unmeasured case,
# misclassification, absent separation, or degenerate score distribution.
#
# Usage:
#   MODEL=~/models/shieldstral/Shieldstral-1.0-3B-Q4_K_M.gguf \
#   MMPROJ=~/models/shieldstral/mmproj-F16.gguf \
#   sh scripts/eval/run-shieldstral-llamacpp.sh
#
# Set ALLOW_TEXT_ONLY=1 to run deliberately without the vision projector.
set -eu

MODEL="${MODEL:-$HOME/models/shieldstral/Shieldstral-1.0-3B-Q4_K_M.gguf}"
MMPROJ="${MMPROJ:-$HOME/models/shieldstral/mmproj-F16.gguf}"
PORT="${PORT:-8081}"
LOG="${LOG:-$HOME/llama-shieldstral.log}"
ALLOW_TEXT_ONLY="${ALLOW_TEXT_ONLY:-0}"

[ -f "$MODEL" ] || { echo "missing model: $MODEL"; exit 2; }
# Without curl the readiness loop below burns its full 300 s and then reports
# "server never became ready" over a log showing a perfectly healthy server.
command -v curl >/dev/null 2>&1 || { echo "curl not found"; exit 6; }
command -v llama-server >/dev/null 2>&1 || { echo "llama-server not found"; exit 6; }

# Build the mmproj argument in "$@" rather than a word-split string: MMPROJ
# defaults under $HOME, and a path containing a space would otherwise be
# silently truncated into two argv entries.
set --
if [ -f "$MMPROJ" ]; then
  set -- --mmproj "$MMPROJ"
  MULTIMODAL=1
  echo "multimodal: ENABLED ($MMPROJ)"
elif [ "$ALLOW_TEXT_ONLY" = "1" ]; then
  MULTIMODAL=0
  echo "multimodal: SKIPPED by ALLOW_TEXT_ONLY=1 — measurement 1's second half is NOT covered"
else
  # Silence here produced a clean-looking PASS covering half the measurement:
  # the vision leg is the stated reason llama.cpp was chosen over Ollama.
  echo "missing vision projector: $MMPROJ"
  echo "  Measurement 1 has two halves and the image half is the one Ollama"
  echo "  could not have given us. Set MMPROJ=<path>, or ALLOW_TEXT_ONLY=1 to"
  echo "  run the text leg alone and accept a partial measurement."
  exit 7
fi

echo "starting llama-server on :$PORT"
# --alias so the API model name is stable; jinja is on by default so the
# GGUF's own chat template is applied -- which is the thing to verify below,
# since an upstream GGUF shipping a broken stub template is a known hazard
# (it bit the Agents-A1 import) and would silently corrupt every score.
llama-server -m "$MODEL" "$@" \
  --alias shieldstral --port "$PORT" --host 127.0.0.1 \
  -c 8192 -ngl 99 --no-webui > "$LOG" 2>&1 &
SRV=$!
# INT/TERM must EXIT, not fall through: without the explicit exit the handler
# killed the server and then execution resumed in the readiness loop, whose
# `kill -0` promptly reported "server died" — presenting an operator's Ctrl-C
# as a server crash, complete with a log tail.
trap 'kill $SRV 2>/dev/null || true' EXIT
trap 'kill $SRV 2>/dev/null || true; exit 130' INT TERM

# Wait for readiness rather than sleeping a guessed interval.
#
# Must require HTTP **200**, not merely a reachable socket: llama.cpp answers
# /health with 503 while the model is still loading, and a bare `curl -s ...
# >/dev/null` treats that as success -- which sends the probe in early and
# surfaces as a bogus "cannot reach backend" failure of the measurement.
ready=""
for _ in $(seq 1 300); do
  # curl already prints 000 on a connection failure, so no `|| echo 000`
  # fallback — that appended a SECOND 000 and made the variable read `000000`.
  code=$(curl -s -m 2 -o /dev/null -w '%{http_code}' \
         "http://127.0.0.1:$PORT/health" 2>/dev/null)
  [ "$code" = "200" ] && { ready=1; break; }
  kill -0 $SRV 2>/dev/null || { echo "server died:"; tail -20 "$LOG"; exit 3; }
  sleep 1
done
[ -n "$ready" ] || { echo "server never became ready:"; tail -20 "$LOG"; exit 4; }

echo
echo "=== chat template preflight (a broken stub template silently ruins the score) ==="
# Capture first, THEN test. `grep ... | head -5 || echo` can never reach the
# fallback: a pipeline's status is the LAST command's, and head always exits 0
# (verified) — so the negative case printed nothing at all and the section
# rendered as a bare header. The check that guards a known silent-corruption
# hazard must be able to fail.
tmpl=$(grep -iE "chat template|chat_template|tmpl" "$LOG" | head -5 || true)
if [ -n "$tmpl" ]; then
  printf '%s\n' "$tmpl"
else
  echo "  FAIL: no chat-template line in $LOG — cannot confirm the GGUF's own"
  echo "  template was applied. A stub template silently corrupts every score,"
  echo "  so this aborts rather than measure garbage."
  exit 5
fi

echo
MM_FLAG=""
[ "$MULTIMODAL" = "1" ] && MM_FLAG="--multimodal"
# `uv run` so the probe's PEP 723 dependency (pillow, for the image leg) is
# resolved rather than silently absent.
if command -v uv >/dev/null 2>&1; then
  uv run "$(dirname "$0")/shieldstral_logprobs_probe.py" \
    --base-url "http://127.0.0.1:$PORT/v1/chat/completions" \
    --model shieldstral $MM_FLAG
else
  echo "note: uv not found — falling back to python3; the image leg needs Pillow"
  python3 "$(dirname "$0")/shieldstral_logprobs_probe.py" \
    --base-url "http://127.0.0.1:$PORT/v1/chat/completions" \
    --model shieldstral $MM_FLAG
fi
