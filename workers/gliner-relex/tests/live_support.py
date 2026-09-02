"""Pure helpers for the real-model test in `test_model_live.py`.

Kept in their own module, free of pytest and free of the filesystem, so
they can be unit-tested on a host that has neither the 1.3 GB weights nor
a GPU — which is most hosts, most of the time. Every function here takes
the environment as an argument rather than reading `os.environ`, so a
test can hand it any environment it likes without monkeypatching.

The resolution rules mirror the Rust side's
`kastellan_tests_common::gliner_weights::weights_dir_candidate`, which is
the single copy the three gliner-relex e2e suites now share. If you change
one, change both — two halves of one gate that disagree about where the
weights live will produce a skip on one side and a run on the other. The
Rust module carries the same warning pointing back here.
"""
from pathlib import Path
from typing import Mapping, Optional

#: The Hugging Face repo id the on-disk snapshot was downloaded from.
#: `GlinerModel.load` takes it as an argument and does not currently use
#: it — the load names `weights_dir` directly and is `local_files_only`,
#: so nothing here can trigger a fetch. Passed anyway to keep the call
#: shape identical to the worker's, and to record which snapshot the
#: weights on disk are meant to be.
MODEL_ID = "knowledgator/gliner-relex-multi-v1.0"

#: Where `scripts/workers/gliner-relex/install.sh` puts the snapshot,
#: relative to the kastellan data dir. Must stay equal to the Rust
#: `gliner_weights::WEIGHTS_SUBPATH`; both are pinned to a literal by a
#: test, because every other assertion builds its expectation from the
#: constant and so cannot see a typo in it.
WEIGHTS_SUBPATH = "workers/gliner-relex/weights/multi-v1.0"

#: Set to turn a missing-weights skip into a failure.
REQUIRE_E2E_VAR = "KASTELLAN_GLINER_RELEX_REQUIRE_E2E"

#: The project-wide flag dialect (#459): `env_flag_enabled` in
#: `core/src/worker_lifecycle/force_route.rs` accepts exactly these,
#: trimmed and case-insensitive. Every operator-facing kastellan flag
#: reads this way, so this one does too.
_TRUTHY = frozenset({"1", "true", "yes", "on"})


def weights_dir_candidate(env: Mapping[str, str]) -> Optional[Path]:
    """Where the weights *should* be on this host, without looking.

    Three sources, most specific first:

    1. ``KASTELLAN_GLINER_RELEX_WEIGHTS_DIR`` — taken **verbatim**. This
       is the daemon-style override, and it already points at the model
       snapshot itself, so `WEIGHTS_SUBPATH` is *not* appended.
    2. ``KASTELLAN_DATA_DIR`` — the kastellan data root; the snapshot
       sits at `<data dir>/WEIGHTS_SUBPATH`.
    3. ``HOME`` — the default data root is `$HOME/.local/share/kastellan`.

    A variable that is set but **empty** counts as unset, at every level.
    Taking an empty value would build a *relative* path, and a relative
    path silently resolves against whatever the current working directory
    happens to be — a skip whose reason names a location nobody ever
    installed to.

    Returns `None` when none of the three is usable, for the same reason.
    This function does no I/O — the caller decides what a non-existent
    directory means.
    """
    explicit = env.get("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR")
    if explicit:
        return Path(explicit)

    data_dir = env.get("KASTELLAN_DATA_DIR")
    if data_dir:
        return Path(data_dir) / WEIGHTS_SUBPATH

    home = env.get("HOME")
    if home:
        return Path(home) / ".local/share/kastellan" / WEIGHTS_SUBPATH

    return None


def require_live(env: Mapping[str, str]) -> bool:
    """True when a real model run was demanded, so a skip must fail.

    Accepts the project's one flag dialect — ``1``/``true``/``yes``/``on``,
    trimmed and case-insensitive — the same set `env_flag_enabled` accepts
    on the Rust side. An operator who has learned that
    `KASTELLAN_GLINER_RELEX_ENABLE=true` works would otherwise write
    `…REQUIRE_E2E=true` here and get it silently ignored: a silent skip
    from the one knob whose entire job is to abolish silent skips.

    Trimming matters for the same reason it does in Rust: writing a flag
    with `echo "1" >> kastellan.env` yields ``"1\\n"``.
    """
    return env.get(REQUIRE_E2E_VAR, "").strip().lower() in _TRUTHY
