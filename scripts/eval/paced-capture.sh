#!/usr/bin/env bash
#
# Materialise the guard calibration corpus ONE ENTRY AT A TIME, pausing
# between entries.
#
# WHY THIS EXISTS, and why a bare `kastellan-cli guard capture` over the
# whole manifest is not enough:
#
#   `guard capture` fires its fetches back to back. web.archive.org
#   throttles that. A throttled response is NOT a clean failure -- it
#   arrives as a transport error (reported as FETCH-FAILED), or, worse,
#   as an HTTP 200 with an empty or truncated body, which under --record
#   would be hashed and pinned AS THE CASE (issue #602).
#
#   Measured on the measurement-3 campaign, 2026-08-23:
#     104 entries, back to back    -> 20 FETCH-FAILED, plus 1 apparent
#                                     drift on cap-085-debian-home
#     the same 21, one at a time,
#     15-second pause              -> 0 FETCH-FAILED
#
#   The drift did NOT clear with pacing: Wayback had redirected the
#   pinned snapshot to a neighbouring capture, which is #603, a real
#   defect in what the hash covers -- not a throttle artefact. Pacing
#   fixes the fetch failures and nothing else. See Finding 3 of
#   `docs/devel/runbooks/2026-08-23-guard-measurement-3.md`, which is
#   where these numbers live and where they will be corrected.
#
#   So the pause is not politeness, it is what makes the run's failures
#   mean what they say. A FETCH-FAILED from a paced run is worth
#   investigating; one from an unpaced run is usually just the throttle.
#
# WHAT PACING DOES NOT BUY. Nothing here can tell a real document from a
# throttled 200 with an empty body -- the hash of garbage is a perfectly
# good hash. Pacing lowers the probability; the SIZE FLOOR below is what
# detects it when it happens anyway. Both are needed.
#
# Usage:
#   scripts/eval/paced-capture.sh <manifest-dir> <out-dir> [--record] [pause-seconds]
#
#   pause-seconds defaults to 8. The campaign's "0 FETCH-FAILED" result
#   was measured at 15; pass 15 to reproduce it. 0 is refused -- an
#   unpaced run is what this script exists to avoid.
#
#   PACED_MIN_BYTES (default 256) is the size floor. An entry whose
#   extracted text comes in under it is reported SUSPECT and counted a
#   failure, because that is what a throttled or truncated body looks
#   like. Raise it for a corpus of substantial documents; lower it only
#   if a legitimately tiny source is being pinned deliberately.
#
# Without --record every entry must print OK: the source still yields the
# bytes whose hash the manifest pins. With --record, new entries print
# RECORD-NEW and already-pinned ones RECORD-SAME -- and are still
# verified, because --record is not a way to skip the check.
#
# The full output of every invocation is kept in <out-dir>.logs/<id>.log
# (a SIBLING of the out dir, so it is not itself read as a corpus).
# Anything the outcome classifier does not recognise -- WRITE-FAILED's
# errno, "cannot create <dir>", the "stale and WILL be scored" warning,
# a panic -- lives there rather than being consumed by the pipe.
#
# After the last entry the out dir is reconciled against the manifest.
# An interrupted run otherwise leaves a SHORT corpus that is gitignored,
# unmarked, and structurally indistinguishable from a complete one.
#
# Exits non-zero if any entry did not succeed, or if any manifest entry
# is absent from the out dir at the end.

set -uo pipefail

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <manifest-dir> <out-dir> [--record] [pause-seconds]" >&2
  exit 2
fi

MANIFEST="$1"
OUT="$2"
shift 2

RECORD=""
PAUSE=8
PAUSE_SET=""
for arg in "$@"; do
  case "$arg" in
    --record)
      RECORD="--record"
      ;;
    ''|*[!0-9]*)
      echo "$0: unrecognised argument '$arg'" >&2
      exit 2
      ;;
    *)
      # Two pause values is a typo, not a preference -- silently taking
      # the last would run at a pace the operator did not ask for.
      if [ -n "$PAUSE_SET" ]; then
        echo "$0: pause given twice ('$PAUSE' then '$arg')" >&2
        exit 2
      fi
      PAUSE="$arg"
      PAUSE_SET=1
      ;;
  esac
done

# A zero pause is the back-to-back behaviour the header measures at 20
# FETCH-FAILED. Accepting it silently would let the script report a
# throttled run as an honest one.
if [ "$PAUSE" -eq 0 ]; then
  echo "$0: a pause of 0 is an unpaced run, which is what this script exists" >&2
  echo "    to avoid -- pass 8 or more (15 reproduces the campaign)" >&2
  exit 2
fi
# BSD `sleep` rejects a value this large outright (rc=1, usage error)
# where GNU accepts it, so an absurd pause silently becomes NO pause on
# macOS. Refuse it on both hosts rather than diverge.
if [ "$PAUSE" -gt 3600 ]; then
  echo "$0: pause '$PAUSE' is over an hour; that is not a pace, it is a hang" >&2
  exit 2
fi

MIN_BYTES="${PACED_MIN_BYTES:-256}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CLI="${KASTELLAN_CLI:-$REPO_ROOT/target/debug/kastellan-cli}"
if [ ! -x "$CLI" ]; then
  echo "$0: $CLI not built -- run 'cargo build --workspace' first," >&2
  echo "    or set KASTELLAN_CLI if you build into a custom target dir" >&2
  exit 1
fi

# `timeout` is GNU coreutils and is NOT in macOS base userland, where it
# arrives only via homebrew (often as `gtimeout`). Without this preflight
# its absence is INVISIBLE: `command not found` goes to stderr, `2>&1`
# folds it into the pipe, the outcome grep drops it, and every entry
# reports NO-OUTCOME with the real cause named nowhere -- 109 of those,
# one per pause, over a quarter of an hour. Same class of misdiagnosis
# the sibling run-shieldstral-llamacpp.sh preflights `curl` for.
TIMEOUT="$(command -v timeout || command -v gtimeout || true)"
if [ -z "$TIMEOUT" ]; then
  echo "$0: neither 'timeout' nor 'gtimeout' on PATH" >&2
  echo "    macOS: brew install coreutils" >&2
  exit 1
fi

if [ ! -d "$MANIFEST" ]; then
  echo "$0: manifest directory '$MANIFEST' does not exist" >&2
  exit 1
fi
mkdir -p "$OUT"

# With --record, `guard capture` writes corpus-shaped {id}.json into the
# out dir, and ids match manifest stems by construction -- so pointing
# OUT at the manifest overwrites every committed entry before saying
# anything useful. Compared after mkdir, and resolved, so `.` and a
# trailing slash cannot slip past it.
if [ "$(cd "$MANIFEST" && pwd -P)" = "$(cd "$OUT" && pwd -P)" ]; then
  echo "$0: out dir must not be the manifest dir -- that would overwrite it" >&2
  exit 2
fi
# A SIBLING of the out dir, never inside it: `guard calibrate` and
# `report_orphans` both read the out dir as a corpus, and a directory of
# logs sitting in it is one more thing they have to be right about
# ignoring.
LOGDIR="${OUT%/}.logs"
mkdir -p "$LOGDIR"

total=0
bad=0
# `guard capture` takes a DIRECTORY, so each entry is handed to it in a
# scratch directory of its own. That is also what keeps one entry's
# failure from aborting the rest.
#
# The authoritative signal is the TOOL's outcome line, not the exit
# status of the pipeline: piping through `grep` would report whether a
# line matched, which a refusal also satisfies. So the line is captured
# and classified.
#
# The FULL output is teed to a per-entry log first. Everything the grep
# does not match -- WRITE-FAILED's errno, `cannot create <dir>`, the
# "stale and WILL be scored" warning, a panic -- would otherwise be
# consumed by the pipe, leaving a bare NO-OUTCOME with no way to
# diagnose it short of re-running the fetch.
for f in "$MANIFEST"/*.json; do
  [ -e "$f" ] || { echo "$0: no *.json in '$MANIFEST'" >&2; exit 1; }
  id=$(basename "$f" .json)
  total=$((total + 1))
  # Between entries, not after the last one.
  [ "$total" -gt 1 ] && sleep "$PAUSE"

  tmp=$(mktemp -d) || { echo "$0: mktemp failed" >&2; exit 1; }
  cp "$f" "$tmp/" || { echo "COPY-FAILED $id"; bad=$((bad + 1)); rm -rf "$tmp"; continue; }
  log="$LOGDIR/$id.log"
  "$TIMEOUT" 180 "$CLI" guard capture --manifest "$tmp" --out "$OUT" $RECORD >"$log" 2>&1
  line=$(grep -E "^(OK|RECORD-NEW|RECORD-SAME|REFUSED|FETCH-FAILED|WRITE-FAILED)" "$log" | head -1)
  rm -rf "$tmp"

  if [ -z "$line" ]; then
    # No recognisable outcome at all: a timeout, a crash, or an output
    # shape change. Counted as bad -- silence must never read as success.
    echo "NO-OUTCOME $id (see $log)"
    bad=$((bad + 1))
    continue
  fi

  case "$line" in
    OK*|RECORD-NEW*|RECORD-SAME*)
      # The tool already printed the extracted size on this line. A
      # throttled 200 yields a near-empty body that hashes and pins
      # perfectly happily (#602), and the byte count is the only thing
      # in the whole pipeline that can tell it from a real document --
      # so it is checked, not just echoed.
      bytes=$(printf '%s\n' "$line" | sed -n 's/.*(\([0-9][0-9]*\) bytes.*/\1/p')
      if [ -z "$bytes" ]; then
        echo "NO-SIZE $id -- outcome line carries no byte count: $line"
        bad=$((bad + 1))
      elif [ "$bytes" -lt "$MIN_BYTES" ]; then
        echo "SUSPECT $id -- $bytes bytes, under the $MIN_BYTES floor (#602: a"
        echo "        throttled 200 with an empty body hashes just fine): $line"
        bad=$((bad + 1))
      else
        echo "$line"
      fi
      ;;
    *)
      echo "$line"
      bad=$((bad + 1))
      ;;
  esac
done

# Reconcile what is ON DISK against what the manifest asked for.
#
# Two reasons, both of which the per-entry outcome lines cannot cover.
# (1) A run interrupted partway -- Ctrl-C during a pause, a killed
#     terminal -- leaves a SHORT corpus that is gitignored, unmarked and
#     structurally indistinguishable from a complete one; `guard
#     calibrate` accepts it and fits a tau over whatever survived.
# (2) `guard capture`'s own orphan check is defeated by this script: it
#     compares the shared out dir against the entries it was handed,
#     which here is a single file, so from the second entry onward it
#     reports the whole rest of the corpus as orphans -- and every one
#     of those lines is filtered out by the outcome grep above.
#
# Missing is a FAILURE. Extra is only a note -- the documented next step
# copies the seeded corpus into this same directory, so extras are
# expected on any re-run after that.
missing=0
for f in "$MANIFEST"/*.json; do
  id=$(basename "$f" .json)
  if [ ! -e "$OUT/$id.json" ]; then
    echo "MISSING $id -- the manifest asked for it and it is not in $OUT"
    missing=$((missing + 1))
  fi
done
# Counted SEPARATELY from `bad`, and reported separately: an entry that
# printed WRITE-FAILED is one failure, not two, and rolling it into the
# same tally would make "N of TOTAL entries did not succeed" print an N
# larger than the number of entries that actually failed.
extra=0
for g in "$OUT"/*.json; do
  [ -e "$g" ] || break
  id=$(basename "$g" .json)
  [ -e "$MANIFEST/$id.json" ] || extra=$((extra + 1))
done
if [ "$extra" -gt 0 ]; then
  echo "NOTE: $extra file(s) in $OUT are not from this manifest."
  echo "      Expected if the seeded corpus has been copied in; otherwise they"
  echo "      WILL be scored by 'guard calibrate' and will move tau."
fi

echo
if [ "$bad" -gt 0 ] || [ "$missing" -gt 0 ]; then
  verdict="$bad of $total entries did NOT succeed"
  if [ "$missing" -gt 0 ]; then
    verdict="$verdict; $missing of $total are absent from the out dir"
  fi
  verdict="$verdict (out dir: $OUT, logs: $LOGDIR)"
  # To BOTH streams: a 15-minute run is normally redirected to a file,
  # and a stderr-only verdict leaves that file ending on the last
  # outcome line with no statement of whether the run succeeded.
  echo "$verdict"
  echo "$verdict" >&2
  exit 1
fi
echo "all $total entries succeeded into $OUT"
