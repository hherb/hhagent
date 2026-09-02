"""Real-model load test — the one thing the mocked suite cannot prove.

Why this file exists
--------------------

Every other test in this directory mocks `gliner.GLiNER` away, because a
real load pulls 1.3 GB of weights off disk. That is the right default,
but it leaves one gap that bit us during the transformers security bump
of issue #649: **a library upgrade can break the model-load path, and a
fully green `pytest` would say nothing about it.** `conftest.py` used to
point at "the manual smoke test (operator-runnable, not in CI)" — which
had never existed as a runnable artifact, only as a one-off someone ran
in May 2026. This file is that smoke test, made repeatable.

How it is gated
---------------

The live test is **skip-as-pass** when the weights are not on this host,
matching the convention `core/tests/entity_extraction_e2e.rs` uses on the
Rust side. A skip prints its reason, so `pytest -rs` shows why.

Skip-as-pass has a known hazard: a test that skipped is indistinguishable
from a test that ran, in the result column. So there is an opt-out —
set ``KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1`` and a missing weights dir
becomes a hard **failure** instead of a skip. Use it whenever a green run
is supposed to *mean* something: after a dependency bump, and in any CI
job that stages the weights.

Run it explicitly with::

    uv run --frozen pytest tests/test_model_live.py -rs

The pure path/env helpers it needs live in `live_support.py` so they can
be unit-tested on a host with no weights at all; those unit tests are in
this file too and always run.
"""
from pathlib import Path

import pytest

from tests.live_support import (
    MODEL_ID,
    WEIGHTS_SUBPATH,
    require_live,
    weights_dir_candidate,
)

# ---------------------------------------------------------------------
# Unit tests for the pure helpers — no weights, no model, always run.
# ---------------------------------------------------------------------


def test_explicit_override_is_used_verbatim():
    """`KASTELLAN_GLINER_RELEX_WEIGHTS_DIR` IS the weights dir.

    It is not a base to append `WEIGHTS_SUBPATH` to. This mirrors the
    Rust `resolve_weights_dir`, where the explicit override is the
    daemon-style verbatim form; getting this backwards would send the
    test looking one level too deep and report a spurious skip.
    """
    env = {"KASTELLAN_GLINER_RELEX_WEIGHTS_DIR": "/opt/weights/multi-v1.0"}
    assert weights_dir_candidate(env) == Path("/opt/weights/multi-v1.0")


def test_data_dir_gets_the_subpath_appended():
    env = {"KASTELLAN_DATA_DIR": "/srv/kastellan"}
    assert weights_dir_candidate(env) == Path("/srv/kastellan") / WEIGHTS_SUBPATH


def test_home_is_the_last_resort():
    env = {"HOME": "/home/agent"}
    expected = Path("/home/agent") / ".local/share/kastellan" / WEIGHTS_SUBPATH
    assert weights_dir_candidate(env) == expected


def test_explicit_override_beats_data_dir():
    """Precedence, asserted rather than assumed.

    Both keys present is the interesting case: a reader can only tell
    which wins by running it, so pin it.
    """
    env = {
        "KASTELLAN_GLINER_RELEX_WEIGHTS_DIR": "/opt/weights/multi-v1.0",
        "KASTELLAN_DATA_DIR": "/srv/kastellan",
    }
    assert weights_dir_candidate(env) == Path("/opt/weights/multi-v1.0")


def test_data_dir_beats_home():
    env = {"KASTELLAN_DATA_DIR": "/srv/kastellan", "HOME": "/home/agent"}
    assert weights_dir_candidate(env) == Path("/srv/kastellan") / WEIGHTS_SUBPATH


def test_no_env_at_all_yields_none():
    """An empty environment must not resolve to a relative path.

    Returning `Path(WEIGHTS_SUBPATH)` here would silently look under the
    current working directory — a skip whose reason names a path that
    was never the right one.
    """
    assert weights_dir_candidate({}) is None


@pytest.mark.parametrize("value", ["1"])
def test_require_live_is_on_for_exactly_one(value):
    assert require_live({"KASTELLAN_GLINER_RELEX_REQUIRE_E2E": value}) is True


@pytest.mark.parametrize("value", ["0", "", "true", "yes", "TRUE", " 1"])
def test_require_live_is_off_for_everything_else(value):
    """Exactly `"1"`, matching the Rust side's `Some("1")` comparison.

    Accepting `"true"` here and not there would mean the two halves of
    the same gate disagree about whether a run was demanded.
    """
    assert require_live({"KASTELLAN_GLINER_RELEX_REQUIRE_E2E": value}) is False


def test_require_live_is_off_when_unset():
    assert require_live({}) is False


# ---------------------------------------------------------------------
# The live test — real weights, real transformers, real inference.
# ---------------------------------------------------------------------

# A sentence chosen so the expected extraction is unambiguous: three
# entities of three distinct types, joined by two relations that are
# stated outright rather than implied. If a library upgrade breaks the
# load or the envelope, this fails; it is not a model-quality probe.
LIVE_TEXT = "Horst Herb maintains the kastellan project, which is written in Rust."
LIVE_ENTITY_LABELS = ["person", "project", "programming language"]
LIVE_RELATION_LABELS = ["maintains", "written in"]


def _weights_dir_or_skip(env) -> Path:
    """Resolve the weights dir, or end the test the way the gate says.

    Skip (with a reason naming the path we looked at) by default; fail
    when `KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1` demanded a real run.
    """
    candidate = weights_dir_candidate(env)
    if candidate is not None and candidate.is_dir():
        return candidate

    where = str(candidate) if candidate is not None else "<no KASTELLAN_DATA_DIR or HOME set>"
    reason = (
        f"gliner-relex weights not found at {where} — "
        "run scripts/workers/gliner-relex/install.sh"
    )
    if require_live(env):
        pytest.fail(f"KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1 but {reason}")
    pytest.skip(reason)


def test_real_model_loads_and_extracts():
    """Load the on-disk weights for real and run one extraction.

    Pinned on purpose:

    * **entities are non-empty** and include the person — the load
      succeeded and inference produced something. An empty result would
      pass a "no exception" test while proving nothing.
    * **every triple's head and tail are among the entities** — the
      envelope invariant `GlinerModel.extract` documents and enforces.
      This is a property, so it stays true across model versions, unlike
      an exact triple list.

    Device is `cpu` rather than `auto`: deterministic on both hosts, and
    it keeps the test off the GPU the daemon may be using.
    """
    import os

    weights = _weights_dir_or_skip(os.environ)

    # Imported inside the test so a host without torch still collects
    # (and runs) the pure helper tests above.
    from kastellan_worker_gliner_relex.model import GlinerModel

    model = GlinerModel.load(str(weights), MODEL_ID, "cpu")
    out = model.extract(
        text=LIVE_TEXT,
        entity_labels=LIVE_ENTITY_LABELS,
        relation_labels=LIVE_RELATION_LABELS,
        threshold=0.5,
        relation_threshold=0.5,
        max_entities=16,
    )

    texts = [e["text"] for e in out["entities"]]
    assert texts, "real model returned no entities — the load path is broken"
    assert any("Horst Herb" in t for t in texts), (
        f"expected the person in {texts!r}"
    )

    surviving = {e["text"] for e in out["entities"]}
    for triple in out["triples"]:
        assert triple["head"]["text"] in surviving, triple
        assert triple["tail"]["text"] in surviving, triple
