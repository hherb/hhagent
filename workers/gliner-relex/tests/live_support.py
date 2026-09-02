"""Pure helpers for the real-model test in `test_model_live.py`.

Kept in their own module, free of pytest and free of the filesystem, so
they can be unit-tested on a host that has neither the 1.3 GB weights nor
a GPU — which is most hosts, most of the time. Every function here takes
the environment as an argument rather than reading `os.environ`, so a
test can hand it any environment it likes without monkeypatching.

The resolution rules deliberately mirror the Rust side's
`core/tests/entity_extraction_e2e.rs::resolve_weights_dir`. If you change
one, change both — two halves of one gate that disagree about where the
weights live will produce a skip on one side and a run on the other.
"""
from pathlib import Path
from typing import Mapping, Optional

#: The Hugging Face repo id the on-disk snapshot was downloaded from.
#: Passed through to `GLiNER.from_pretrained` for provenance only — the
#: load itself is `local_files_only`, so this never causes a fetch.
MODEL_ID = "knowledgator/gliner-relex-multi-v1.0"

#: Where `scripts/workers/gliner-relex/install.sh` puts the snapshot,
#: relative to the kastellan data dir.
WEIGHTS_SUBPATH = "workers/gliner-relex/weights/multi-v1.0"

#: Set to exactly "1" to turn a missing-weights skip into a failure.
REQUIRE_E2E_VAR = "KASTELLAN_GLINER_RELEX_REQUIRE_E2E"


def weights_dir_candidate(env: Mapping[str, str]) -> Optional[Path]:
    """Where the weights *should* be on this host, without looking.

    Three sources, most specific first:

    1. ``KASTELLAN_GLINER_RELEX_WEIGHTS_DIR`` — taken **verbatim**. This
       is the daemon-style override, and it already points at the model
       snapshot itself, so `WEIGHTS_SUBPATH` is *not* appended.
    2. ``KASTELLAN_DATA_DIR`` — the kastellan data root; the snapshot
       sits at `<data dir>/WEIGHTS_SUBPATH`.
    3. ``HOME`` — the default data root is `$HOME/.local/share/kastellan`.

    Returns `None` when none of the three is set, rather than a relative
    path: a relative path would silently resolve against whatever the
    current working directory happens to be, and the resulting skip
    message would name a location nobody ever installed to.

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

    The comparison is against the exact string ``"1"``, matching the
    Rust side's `Some("1")` check on its own opt-in variables. Accepting
    ``"true"`` here and not there would let one half of the gate believe
    a run was demanded while the other half quietly skipped.
    """
    return env.get(REQUIRE_E2E_VAR) == "1"
