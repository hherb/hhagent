#!/usr/bin/env bash
# Rebuild every micro-VM rootfs image (issue #667).
#
# WHY THIS EXISTS
#
# Every rootfs image bakes its OWN copy of `kastellan-microvm-init` and of its
# worker binary, so a change to the guest init or to a worker is invisible to
# the Firecracker e2es until the affected image is rebuilt. The suites now
# refuse to boot a stale image (`tests-common/src/microvm/freshness.rs`), and
# this is the one command that clears that refusal for all of them.
#
# It also removes a real trap: the build scripts live in TWO directories
# (`scripts/workers/microvm/` for seven of them, `scripts/workers/kv-demo/`
# for the eighth), which makes "rebuild everything" easy to get wrong by
# hand-listing paths — the #667 session did exactly that, twice.
#
# USAGE
#
#   bash scripts/workers/microvm/rebuild-all-rootfs.sh              # all images
#   bash scripts/workers/microvm/rebuild-all-rootfs.sh kv-demo web-fetch
#
# An argument selects images by the stem of their `.ext4` name. Unknown names
# are refused up front rather than silently building nothing.
#
# Every image is attempted even when an earlier one fails, and the exit status
# is non-zero if ANY failed — a partial rebuild that reported success would
# leave exactly the stale image this script exists to prevent.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash: bash scripts/workers/microvm/rebuild-all-rootfs.sh" >&2; exit 1
fi
set -uo pipefail

# The build scripts use repo-relative paths (`target/release/...`), so they
# must run from the workspace root whatever directory the operator invoked
# this from.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO_ROOT"

# image stem -> build script. Kept in the SAME ORDER and with the same paths as
# `ROOTFS_IMAGES` in `tests-common/src/microvm/images.rs`; the unit test
# `the_rebuild_all_script_covers_every_registered_image` fails if an entry
# there is missing here, so a new image cannot be added without landing in
# this list too.
IMAGES=(
    "python-exec:scripts/workers/microvm/build-rootfs.sh"
    "web-fetch:scripts/workers/microvm/build-web-fetch-rootfs.sh"
    "web-search:scripts/workers/microvm/build-web-search-rootfs.sh"
    "web-research:scripts/workers/microvm/build-web-research-rootfs.sh"
    "browser-driver:scripts/workers/microvm/build-browser-driver-rootfs.sh"
    "matrix:scripts/workers/microvm/build-matrix-rootfs.sh"
    "net-demo:scripts/workers/microvm/build-net-demo-rootfs.sh"
    "kv-demo:scripts/workers/kv-demo/build-kv-demo-rootfs.sh"
)

stem_of()   { echo "${1%%:*}"; }
script_of() { echo "${1#*:}"; }

# Refuse an unknown selector up front. Building nothing and exiting 0 would be
# the same class of silent no-op this script is meant to end.
selected=("$@")
if [ ${#selected[@]} -gt 0 ]; then
    for want in "${selected[@]}"; do
        found=no
        for entry in "${IMAGES[@]}"; do
            [ "$(stem_of "$entry")" = "$want" ] && found=yes && break
        done
        if [ "$found" = no ]; then
            echo "Unknown image: $want" >&2
            echo "Known: $(for e in "${IMAGES[@]}"; do printf '%s ' "$(stem_of "$e")"; done)" >&2
            exit 2
        fi
    done
fi

wanted() {
    [ ${#selected[@]} -eq 0 ] && return 0
    for want in "${selected[@]}"; do
        [ "$want" = "$1" ] && return 0
    done
    return 1
}

built=() ; failed=() ; skipped=()
for entry in "${IMAGES[@]}"; do
    stem="$(stem_of "$entry")"
    script="$(script_of "$entry")"
    if ! wanted "$stem"; then
        skipped+=("$stem")
        continue
    fi
    if [ ! -f "$script" ]; then
        echo "MISSING build script for $stem: $script" >&2
        failed+=("$stem")
        continue
    fi
    echo
    echo "=== rebuilding ${stem}.ext4 — bash $script"
    if bash "$script"; then
        built+=("$stem")
    else
        # Keep going: one image needing docker (browser-driver) must not stop
        # the other seven from being brought up to date.
        echo "FAILED: $stem (see the output above)" >&2
        failed+=("$stem")
    fi
done

echo
echo "=== rootfs rebuild summary"
echo "  built:   ${built[*]:-(none)}"
[ ${#skipped[@]} -gt 0 ] && echo "  skipped: ${skipped[*]}"
if [ ${#failed[@]} -gt 0 ]; then
    echo "  FAILED:  ${failed[*]}" >&2
    echo
    echo "A stale image gates nothing (#667): the Firecracker e2es will refuse to" >&2
    echo "boot the images listed above until their rebuild succeeds." >&2
    exit 1
fi
echo "  failed:  (none)"
