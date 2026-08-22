# shellcheck shell=bash
#
# The pinned Shieldstral guard weights — operator-side verification.
#
# SOURCE this file, do not execute it:
#
#     source "$(dirname "${BASH_SOURCE[0]}")/lib/guard-weights.sh"
#     require_guard_weights "$MODEL_PATH"
#
# ---------------------------------------------------------------------
# Why this file exists (issue #592)
# ---------------------------------------------------------------------
#
# Every document in the tree said both hosts ran the same guard model —
# "Runtime + quantisation PINNED ... Shieldstral-1.0-3B-Q8_0 ... on BOTH
# hosts". Measured 2026-08-22, they did not. The Mac held upstream's
# file; the DGX held a DIFFERENT Q8_0 build at the IDENTICAL byte
# length, with a well-formed GGUF header naming the right model and the
# right licence. It loaded, it served, and it produced correct verdicts.
#
# Pinning a quantisation LABEL is not pinning the bytes. Q8_0's size is
# determined by tensor shapes, so two independent conversions of one
# model agree on size while differing in metadata, tensor ordering or
# imatrix use. Size cannot discriminate them; only a hash can.
#
# It matters because the measurement-3 corpus design fits tau on each
# host and compares the two AS A TEST of the cross-platform claim. Run
# against two different builds, either outcome is uninterpretable.
#
# ---------------------------------------------------------------------
# The Rust half, and why the sum is written down twice
# ---------------------------------------------------------------------
#
# `core/src/cassandra/guard_model/weights_pin/mod.rs` carries the same sum
# and does the check `guard calibrate` performs automatically, by asking
# llama-server's /props which file it opened and hashing that.
#
# This file is the operator-facing half: a pre-flight you can run before
# starting llama-server, on a host where the tool is not built, or
# against a file you just downloaded.
#
# The duplication is deliberate — `kastellan-core` is published to
# crates.io and cannot `include_str!` a path outside its own directory,
# the same constraint `scripts/workers/microvm/lib/guest-kernel.sh`
# lives under — and it is CI-ENFORCED rather than hoped-for:
# `kastellan-tests-common`'s `rust_and_bash_guard_pins_agree` fails the
# PR if the two ever drift.
#
# ---------------------------------------------------------------------
# On trusting this sum
# ---------------------------------------------------------------------
#
# It is HuggingFace's canonical LFS oid for the upstream file, which is
# a stronger provenance than the guest kernel's trust-on-first-use: the
# value was not merely observed on a host, it matches what the
# publishing repository states. It is still not a signature — upstream
# publishes none — so this pins against drift and tampering after the
# fact, not against the possibility that the published file was never
# honest.

# sha256 of upstream's Shieldstral-1.0-3B-Q8_0.gguf.
#
# Must equal PINNED_SHA256 in core/src/cassandra/guard_model/weights_pin/mod.rs.
# Bump both together, in the same deliberate step as the model change,
# and re-run measurement 3 — a tau fitted on other weights does not
# transfer. Never "fix" a mismatch by pasting in whatever a failure
# printed.
KASTELLAN_GUARD_WEIGHTS_SHA256="35b755bed2d473fb3d88f7d1d7b83203bd9f0c1b8bff42624e6fd9231d89d3c4"

# Byte length of the pinned file. NOT part of the check — #592's whole
# point is that the wrong file had this exact length — but reported on a
# mismatch, because same-size means a different quantiser run of the
# right model while a different size means the wrong file entirely.
KASTELLAN_GUARD_WEIGHTS_SIZE_BYTES="3651679008"

# Where to fetch the pinned file.
KASTELLAN_GUARD_WEIGHTS_SOURCE="https://huggingface.co/noctrex/Shieldstral-1.0-3B-GGUF"

# Print the sha256 of "$1" on stdout, portably.
#
# `sha256sum` is GNU (Linux); `shasum -a 256` is what macOS ships. Both
# hosts are first-class here, so neither is assumed — the same
# portability rule the rest of the tree follows. `shasum` WITHOUT
# `-a 256` is SHA-1, so the flag is not optional.
#
# The hasher's output is captured and its status checked rather than
# piped into `cut`. A pipeline's status is its LAST command's, so
# `sha256sum "$f" | cut …` returns 0 even when the hash never happened
# — and this file is meant to be `source`d, so it cannot rely on the
# caller having set `pipefail`. Found in review: with an unreadable
# file the caller's `|| return 1` was dead code, `$actual` came back
# empty, and the mismatch branch confidently reported "a different file
# altogether" for what was a read error. It failed closed only because
# the empty string is not the pin.
_kastellan_sha256_of() {
    local target="${1:-}" line
    if command -v sha256sum >/dev/null 2>&1; then
        line="$(sha256sum "$target")" || return 1
    elif command -v shasum >/dev/null 2>&1; then
        line="$(shasum -a 256 "$target")" || return 1
    else
        echo "guard-weights: neither sha256sum nor shasum is available" >&2
        return 1
    fi
    # Both tools print "<sum>  <path>"; strip at the first space.
    printf '%s\n' "${line%% *}"
}

# Verify that "$1" is the pinned guard-model file.
#
# VERIFY ONLY: never downloads, never repairs. Returns 0 when the file
# matches and non-zero otherwise, printing what to do about it.
require_guard_weights() {
    # `${1:-}` rather than `$1`: under a caller's `set -u` a bare `$1`
    # aborts the shell before the "no path given" branch below can
    # explain itself, turning a documented rc=2 into an unbound-variable
    # trace. It still fails closed either way; this makes it diagnosable.
    local target="${1:-}"

    if [ -z "$target" ]; then
        echo "require_guard_weights: no path given" >&2
        return 2
    fi
    # The one shape that would fail OPEN: an empty pin makes the
    # comparison below `[ "$actual" = "" ]`, which succeeds for a file
    # that was never hashed. Reachable if a caller overrides the
    # variable, or defines this function without sourcing the file.
    if [ -z "$KASTELLAN_GUARD_WEIGHTS_SHA256" ]; then
        echo "require_guard_weights: the pin constant is empty -- was this file sourced?" >&2
        return 2
    fi
    if [ ! -f "$target" ]; then
        echo "require_guard_weights: no such file: $target" >&2
        echo "  fetch it from $KASTELLAN_GUARD_WEIGHTS_SOURCE" >&2
        return 1
    fi

    local actual
    actual="$(_kastellan_sha256_of "$target")" || {
        echo "require_guard_weights: could not hash $target" >&2
        echo "  This is a READ failure, not a wrong-model finding -- check" >&2
        echo "  permissions and the device before hunting for the right file." >&2
        return 1
    }
    if [ -z "$actual" ]; then
        echo "require_guard_weights: the hasher produced no output for $target" >&2
        return 1
    fi

    if [ "$actual" = "$KASTELLAN_GUARD_WEIGHTS_SHA256" ]; then
        return 0
    fi

    local actual_size
    actual_size="$(wc -c < "$target" | tr -d ' ')"

    echo "require_guard_weights: $target is NOT the pinned guard model." >&2
    echo "  expected: $KASTELLAN_GUARD_WEIGHTS_SHA256" >&2
    echo "  actual:   $actual ($actual_size bytes)" >&2
    if [ "$actual_size" = "$KASTELLAN_GUARD_WEIGHTS_SIZE_BYTES" ]; then
        echo "  Same size as the pinned file, so this is a DIFFERENT QUANTISER RUN" >&2
        echo "  of the right model -- not corruption, and not the wrong model." >&2
    else
        echo "  Different size from the pinned file, so this is a different file" >&2
        echo "  altogether -- check the path is the one you meant." >&2
    fi
    echo "  Fetch the pinned file from $KASTELLAN_GUARD_WEIGHTS_SOURCE" >&2
    echo "  If the model was changed on purpose, update BOTH this file and" >&2
    echo "  core/src/cassandra/guard_model/weights_pin/mod.rs, and re-run" >&2
    echo "  measurement 3. See issue #592." >&2
    return 1
}
