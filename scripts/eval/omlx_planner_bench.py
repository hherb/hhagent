#!/usr/bin/env python3
"""Benchmark an oMLX-served chat model on the job kastellan actually gives it:
formulating a plan from the real `prompts/agent_planner.md` system prompt.

Why not a generic benchmark: the model's score on someone else's eval says
nothing about the two things that decide whether a task succeeds here --
(a) whether the completion parses as a plan at all, and (b) whether it
arrives inside `KASTELLAN_LLM_TIMEOUT_MS`. Both have bitten this project
before; see the `plan_iteration_cap_exceeded` and `plan decode failed`
history in the handover.

Measures, per configuration:
  * time to first token and decode throughput
  * total wall clock against the router's 180 s budget
  * whether the completion is a VALID plan -- the keys the DAEMON requires
    (`REQUIRED_PLAN_KEYS`), with `steps` a list; `missing field 'steps'` is
    the rejection that has actually bitten this project
  * how much of the generation went to thinking rather than the plan,
    counting both `<think>` tags in `content` and the separate
    `reasoning_content` stream

The thinking comparison is the point of the exercise. Qwen3.8 reports
`thinking_default: true`, and `RouterConfig::disable_thinking` defaults ON
and emits `chat_template_kwargs: {"enable_thinking": false}` on every local
chat completion. If oMLX ignores that key, the default macOS backend
silently runs in the configuration that cost 222 s/plan on the DGX.

Stdlib only.

Usage:
    python3 omlx_planner_bench.py --model Qwen3.8-27B-8bit [--repeat 2]
    python3 omlx_planner_bench.py --cache-probe     # cold-vs-warm numbers

## Measured 2026-08-15 -- Qwen3.8-27B-8bit on oMLX, M-series Mac
##
##   thinking OFF : median 29.7 s, max 34.1 s, 2/2 valid, 0 over budget
##   thinking ON  : median 50.8 s, max 51.5 s, 2/2 valid, 0 over budget
##   ratio        : 1.71x  => oMLX DOES honour chat_template_kwargs, so the
##                  router's `disable_thinking` default is load-bearing here
##   plan validity: 4/4 across both tasks and both settings
##
## Two tasks x one repeat: a smoke test, not a distribution.
##
## Prompt cache is the biggest single lever and is invisible to the default
## run above, which times only cold calls. Reproduce with `--cache-probe`;
## the numbers below came from that path:
##   call 1  36.6 s  cached=0     (4402 prompt tokens, all cold)
##   call 2  14.6 s  cached=4096
##   call 3  14.4 s  cached=4096
## kastellan re-sends an identical planner system prompt every call, so the
## WARM number is the operational one -- a 2.5x difference. Benchmark the
## second call, never the first.
##
## With thinking disabled the completion starts with `{` at offset 0: no
## preamble for `parse_plan_lenient` to skip. With `max_tokens` set very low
## and no enable_thinking kwarg, raw reasoning prose leaks into `content`
## instead ("We need to respond to user: ..."), which is the shape that
## surfaces downstream as `plan decode failed`.
"""

import argparse
import json
import re
import statistics
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

# What the DAEMON requires, not what a strict reading of the prompt asks for.
# `data_ceiling` is deliberately absent: `cassandra::types::Plan` marks it
# `Option` + `serde(default)` and its field doc notes the model omits it
# "fairly often on terminal plans", so grading it as mandatory would score a
# plan INVALID that the daemon accepts and resolves against the floor -- and
# the `terminal` task below is exactly that shape. The rejection that has
# actually bitten this project is `missing field 'steps'` (ROADMAP #505).
REQUIRED_PLAN_KEYS = ("context", "decision", "rationale", "steps")

# Two tasks: one that should terminate immediately (tests the minimum
# terminal plan, the shape the prompt says is most often got wrong), and
# one needing a tool step (tests that it uses the advertised tool surface).
TASKS = {
    "terminal": {
        "instruction": "What is 17 multiplied by 23? Answer directly.",
        "classification_floor": "Public",
        "plans_so_far": [],
        "advisories": [],
        "blocks": [],
    },
    "tool-step": {
        "instruction": "Find the three most recent emails from Qantas and tell me the booking reference.",
        "classification_floor": "Personal",
        "plans_so_far": [],
        "advisories": [],
        "blocks": [],
    },
}

# A representative <tools> block. Kept small and honest -- this is not the
# live registry, so the benchmark measures format-following, not recall of
# the real deployment.
TOOLS_BLOCK = """<tools>
- mail (method: search): search the user's mail archive.
  params: query (string), sort ("rank" | "date"), limit (integer)
- mail (method: get_message): fetch one message by id.
  params: message_id (integer)
- web (method: search): search the web.
  params: query (string)
</tools>"""


def post(base_url: str, payload: dict, timeout: float = 300.0):
    req = urllib.request.Request(
        base_url,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
    )
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        body = json.loads(resp.read())
    return body, time.perf_counter() - t0


def stream_first_token(base_url: str, payload: dict, timeout: float = 300.0):
    """Measure TTFT properly, by streaming.

    Returns (ttft, total, text, reasoning). `reasoning` is the separate
    `delta.reasoning_content` channel: Qwen3-family models on OpenAI-compat
    servers commonly put thinking there rather than inlining `<think>` tags
    in `content`. Collecting only `content` reported `think=0%` for every
    thinking-ON run as though it had been measured, and undercounted the
    generated volume those runs actually paid for.

    Raises RuntimeError on a server error frame. Such a frame is valid JSON,
    so `obj.get("choices", [{}])[0]` yields `{}` and the piece is `""` --
    dropped silently, after which the run is reported "INVALID (empty
    completion)", blaming the model for a server-side failure.
    """
    p = dict(payload)
    p["stream"] = True
    req = urllib.request.Request(
        base_url,
        data=json.dumps(p).encode(),
        headers={"Content-Type": "application/json"},
    )
    t0 = time.perf_counter()
    ttft = None
    chunks, thinking, undecodable = [], [], 0
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        for raw in resp:
            line = raw.decode(errors="replace").strip()
            if not line.startswith("data:"):
                continue
            data = line[5:].strip()
            if data == "[DONE]":
                break
            try:
                obj = json.loads(data)
            except json.JSONDecodeError:
                undecodable += 1
                continue
            if "error" in obj:
                raise RuntimeError(f"backend error frame: {obj['error']}")
            delta = obj.get("choices", [{}])[0].get("delta", {})
            piece = delta.get("content") or ""
            reasoning = delta.get("reasoning_content") or ""
            if reasoning:
                thinking.append(reasoning)
            if piece or reasoning:
                if ttft is None:
                    ttft = time.perf_counter() - t0
            if piece:
                chunks.append(piece)
    if undecodable:
        print(f"    ({undecodable} undecodable SSE frame(s) skipped)", file=sys.stderr)
    return ttft, time.perf_counter() - t0, "".join(chunks), "".join(thinking)


def extract_plan(text: str):
    """Mirror what `parse_plan_lenient` tolerates: a fenced block, or the
    first balanced {...}. Returns (plan_dict_or_None, note)."""
    if not text:
        return None, "empty completion"
    fence = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", text, re.S)
    candidate = fence.group(1) if fence else None
    if candidate is None:
        start = text.find("{")
        if start == -1:
            return None, "no '{' in completion"
        depth, end = 0, None
        for i, ch in enumerate(text[start:], start):
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    end = i + 1
                    break
        if end is None:
            return None, "unbalanced braces (likely truncated)"
        candidate = text[start:end]
    try:
        return json.loads(candidate), "ok"
    except json.JSONDecodeError as e:
        return None, f"JSON decode failed: {e}"


def grade(plan, note):
    if plan is None:
        return False, note
    missing = [k for k in REQUIRED_PLAN_KEYS if k not in plan]
    if missing:
        return False, f"missing required key(s): {', '.join(missing)}"
    if not isinstance(plan.get("steps"), list):
        return False, "`steps` is not a list"
    return True, "valid plan"


def thinking_share(text: str, reasoning: str = ""):
    """Fraction of the generation spent thinking rather than planning.

    Counts BOTH channels a backend can use: `<think>` tags inlined in
    `content`, and the separate `reasoning_content` stream. Measuring only
    the first reports a flat 0% against any server that uses the second,
    which is the majority of them for this model family.
    """
    inline = sum(len(x) for x in re.findall(r"<think>(.*?)</think>", text, re.S))
    total = len(text) + len(reasoning)
    if total == 0:
        return 0.0
    return (inline + len(reasoning)) / total


def run(base_url, model, system_prompt, task_name, task, disable_thinking, timeout_ms):
    user = json.dumps(task, indent=2)
    payload = {
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user},
        ],
        "temperature": 0.2,
        "max_tokens": 4096,
    }
    if disable_thinking:
        # Exactly what `ChatRequest::without_thinking` puts on the wire.
        payload["chat_template_kwargs"] = {"enable_thinking": False}

    ttft, total, text, reasoning = stream_first_token(
        base_url, payload, timeout=timeout_ms / 1000.0
    )
    plan, note = extract_plan(text)
    ok, why = grade(plan, note)
    approx_tokens = max((len(text) + len(reasoning)) // 4, 1)
    decode_tps = approx_tokens / max(total - (ttft or 0), 1e-6)
    return {
        "task": task_name,
        "thinking_disabled": disable_thinking,
        "ttft": ttft,
        "total": total,
        "chars": len(text),
        "approx_tok_s": decode_tps,
        "valid": ok,
        "why": why,
        "think_share": thinking_share(text, reasoning),
        "over_budget": total > timeout_ms / 1000.0,
        "text": text,
    }


def run_cache_probe(base_url, model, system_prompt, timeout_ms, calls: int = 3):
    """Send the SAME system prompt `calls` times and report prompt-cache reuse.

    This is the biggest single lever on plan latency and is invisible to a
    one-shot timing: kastellan re-sends an identical planner system prompt on
    every call, so the WARM number is the operational one. Non-streaming on
    purpose -- `usage.prompt_tokens_details.cached_tokens` only comes back on
    the final (non-streamed) body, which is why `post` exists.
    """
    print("\n--- prompt-cache probe (identical system prompt, "
          f"{calls} calls) ---")
    payload = {
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": json.dumps(TASKS["terminal"], indent=2)},
        ],
        "temperature": 0.2,
        "max_tokens": 512,
        "chat_template_kwargs": {"enable_thinking": False},
    }
    for i in range(1, calls + 1):
        try:
            body, elapsed = post(base_url, payload, timeout=timeout_ms / 1000.0)
        except (urllib.error.URLError, TimeoutError, OSError) as e:
            print(f"  call {i}: FAILED {type(e).__name__}: {e}")
            continue
        usage = body.get("usage", {}) or {}
        cached = (usage.get("prompt_tokens_details") or {}).get("cached_tokens")
        prompt_toks = usage.get("prompt_tokens")
        cached_s = "not reported" if cached is None else str(cached)
        print(f"  call {i}: {elapsed:6.1f} s   prompt_tokens={prompt_toks}   "
              f"cached={cached_s}")
    print("  Benchmark the SECOND call, never the first.")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://127.0.0.1:8000/v1/chat/completions")
    ap.add_argument("--model", default="Qwen3.8-27B-8bit")
    ap.add_argument("--prompt", default="prompts/agent_planner.md")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--timeout-ms", type=int, default=180_000,
                    help="the router's KASTELLAN_LLM_TIMEOUT_MS budget")
    ap.add_argument("--cache-probe", action="store_true",
                    help="also re-send one identical prompt 3x and report "
                         "prompt-cache reuse (reproduces the docstring's "
                         "cold-vs-warm numbers)")
    args = ap.parse_args()

    p = Path(args.prompt)
    if not p.exists():
        print(f"planner prompt not found: {p} (run from the repo root)")
        return 2
    system_prompt = TOOLS_BLOCK + "\n\n" + p.read_text()

    print(f"model        : {args.model}")
    print(f"backend      : {args.base_url}")
    print(f"system prompt: {len(system_prompt)} chars (~{len(system_prompt)//4} tok)")
    print(f"budget       : {args.timeout_ms} ms\n")

    results = []
    for task_name, task in TASKS.items():
        for disable in (True, False):
            for _ in range(args.repeat):
                try:
                    r = run(args.base_url, args.model, system_prompt,
                            task_name, task, disable, args.timeout_ms)
                # A read-phase timeout raises a bare `TimeoutError` (an
                # OSError) -- only CONNECT-phase timeouts get wrapped in
                # URLError. Catching URLError alone therefore let the single
                # most interesting outcome, "the plan blew the router's
                # budget", kill the script with a traceback and discard every
                # result already collected. That is the exact condition this
                # benchmark exists to measure, so it is recorded, not raised.
                except (urllib.error.URLError, TimeoutError, OSError, RuntimeError) as e:
                    print(f"FAIL {task_name} thinking_disabled={disable}: "
                          f"{type(e).__name__}: {e}")
                    results.append({
                        "task": task_name, "thinking_disabled": disable,
                        "ttft": None, "total": args.timeout_ms / 1000.0,
                        "chars": 0, "approx_tok_s": 0.0, "valid": False,
                        "why": f"{type(e).__name__}: {e}", "think_share": 0.0,
                        "over_budget": True, "text": "",
                    })
                    continue
                results.append(r)
                th = "OFF" if disable else "ON "
                ttft = f"{r['ttft']*1000:6.0f}" if r["ttft"] else "   n/a"
                print(f"{r['task']:<10} thinking={th}  ttft={ttft} ms  "
                      f"total={r['total']:6.1f} s  ~{r['approx_tok_s']:5.1f} tok/s  "
                      f"{'VALID' if r['valid'] else 'INVALID'}  "
                      f"think={r['think_share']*100:.0f}%  ({r['why']})")

    if not results:
        return 1

    print("\n--- summary ---")
    for disable in (True, False):
        sub = [r for r in results if r["thinking_disabled"] == disable]
        if not sub:
            continue
        tot = [r["total"] for r in sub]
        valid = sum(1 for r in sub if r["valid"])
        # Read the recorded flag, not `total > budget`: a run that DIED on the
        # budget records `total == budget` exactly, and `>` would score that
        # as inside it — turning the most important failure into a zero.
        over = sum(1 for r in sub if r["over_budget"])
        print(f"thinking {'OFF' if disable else 'ON '}: "
              f"median {statistics.median(tot):6.1f} s   "
              f"max {max(tot):6.1f} s   "
              f"valid {valid}/{len(sub)}   "
              f"over-budget {over}/{len(sub)}")

    off = [r["total"] for r in results if r["thinking_disabled"]]
    on = [r["total"] for r in results if not r["thinking_disabled"]]
    if off and on:
        ratio = statistics.median(on) / max(statistics.median(off), 1e-9)
        print(f"\nthinking ON is {ratio:.2f}x the wall clock of thinking OFF.")
        if ratio < 1.15:
            print("  ** Near parity. Either the model ignores the switch, or oMLX does not")
            print("     forward `chat_template_kwargs` to the template. Verify before")
            print("     relying on `disable_thinking` on this backend.")
        else:
            print("  oMLX honours `chat_template_kwargs.enable_thinking` -- the router's")
            print("  `disable_thinking` default is doing real work on this backend.")

    if args.cache_probe:
        run_cache_probe(args.base_url, args.model, system_prompt, args.timeout_ms)
    return 0


if __name__ == "__main__":
    sys.exit(main())
