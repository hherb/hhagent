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
#
# Exit status is the MEASUREMENT's verdict (see above), except for these
# setup refusals: 2 missing model, 3 server died, 4 server never ready,
# 5 chat-template preflight failed, 6 missing curl/llama-server,
# 7 missing vision projector, 8 port already occupied by another server.
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

# Refuse to measure against a server this script did not start.
#
# If the port is already occupied, our llama-server cannot bind and dies, while
# the readiness probe below cheerfully takes its 200 from the STRANGER — and the
# run then reports a clean pass over unknown weights: the wrong quantisation, or
# a different model entirely. There is no way to tell from the output, because
# --alias makes every Shieldstral server answer to the same name. Silent-wrong
# is the worst outcome available to a measurement, so this fails loudly instead.
occupied=$(curl -s -m 2 -o /dev/null -w '%{http_code}' \
           "http://127.0.0.1:$PORT/health" 2>/dev/null || true)
if [ "$occupied" != "000" ]; then
  echo "port $PORT is already answering (HTTP $occupied)."
  echo "  Refusing to measure against a server this script did not start —"
  echo "  the weights would be whatever that server loaded, not \$MODEL."
  echo "  Stop it, or re-run with PORT=<other>."
  exit 8
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
  # Two separate things about curl on a refused connection, and getting only
  # the first one right is what made this loop unable to wait:
  #  - stdout: it already prints 000, so no `|| echo 000` fallback — that
  #    appended a SECOND 000 and made the variable read `000000`.
  #  - exit status: it EXITS 7 ("failed to connect"), and under `set -e` a
  #    command substitution's status becomes the assignment's, so the script
  #    died on iteration 1 with curl's own 7 — before the model had finished
  #    loading, every time. The loop could therefore only ever succeed against
  #    a server someone else had already started, which is precisely the state
  #    that hid this: the measurement it wraps was run by hand.
  # `|| true` guards the status; the 000 on stdout is what the check below
  # reads, so the not-yet-listening case stays a normal loop iteration.
  code=$(curl -s -m 2 -o /dev/null -w '%{http_code}' \
         "http://127.0.0.1:$PORT/health" 2>/dev/null || true)
  [ "$code" = "200" ] && { ready=1; break; }
  kill -0 $SRV 2>/dev/null || { echo "server died:"; tail -20 "$LOG"; exit 3; }
  sleep 1
done
[ -n "$ready" ] || { echo "server never became ready:"; tail -20 "$LOG"; exit 4; }

echo
echo "=== chat template preflight (a broken stub template silently ruins the score) ==="
# Ask the SERVER which template it will apply, instead of grepping its startup
# log for one.
#
# The log-grep version of this check was unsound in a way that only shows up on
# another build: llama.cpp's startup wording is build-dependent, and the build
# measured here prints no chat-template line at all, at any verbosity. So a grep
# miss was indistinguishable from "this GGUF carries no template" — collapsing
# a clean model and the exact silent-corruption hazard being guarded into one
# output, in the abort direction. (Its predecessor was worse still: `grep | head
# || echo` could never reach the fallback at all, since a pipeline takes head's
# always-zero status.)
#
# /props reports the template actually in force, which is the claim the check
# wants to make. Two failure shapes are rejected:
#   - absent/empty          -> nothing was loaded
#   - present but not Mistral's -> llama.cpp substituted its built-in fallback
#     (chatml), which parses fine and silently reframes every <Instruct> block
tmpl_len=$(curl -s -m 10 "http://127.0.0.1:$PORT/props" 2>/dev/null \
  | python3 -c '
import json, sys
try:
    t = json.load(sys.stdin).get("chat_template") or ""
except Exception:
    t = ""
# The marker is Mistral-specific and measured present in both the noctrex Q4_K_M
# and Abiray Q8_0 GGUFs; a chatml fallback carries neither it nor [INST].
print(len(t) if ("[SYSTEM_PROMPT]" in t or "[INST]" in t) else 0)
' 2>/dev/null || echo 0)
if [ "${tmpl_len:-0}" -gt 0 ]; then
  echo "  OK: server reports a Mistral chat template, ${tmpl_len} chars"
else
  echo "  FAIL: /props reports no Mistral chat template on :$PORT — either none"
  echo "  was embedded in $MODEL, or llama.cpp fell back to its built-in chatml."
  echo "  Both silently corrupt every score, so this aborts rather than measure"
  echo "  garbage. Inspect: curl -s http://127.0.0.1:$PORT/props"
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
