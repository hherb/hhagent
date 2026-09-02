"""Real-model load test — the one thing the mocked suite cannot prove.

Why this file exists
--------------------

Every other test in this directory mocks `gliner.GLiNER` away, because a
real load pulls 1.3 GB of weights off disk. That is the right default,
but it leaves a gap that bit us during the transformers security bump of
issue #649: **a library upgrade can break the model-load path or rename a
field on the wire, and a fully green `pytest` would say nothing about
it.** The README's "Smoke test" section has always covered that by hand —
one `echo … | uv run kastellan-worker-gliner-relex` an operator has to
remember to run, and which caught exactly this class of break once before
(the `type`-not-`label` nesting, 2026-05-18, fixed in `1c36f56`). This
file is the model-load half of that smoke test, made repeatable.

What it pins, and why those things
----------------------------------

Three properties, chosen so they survive a model *version* change but not
a broken load:

* **entities and triples are both non-empty** — a "no exception" test
  would pass on an empty result while proving nothing, and relation
  extraction is the half most likely to break quietly.
* **the wire field keys** required by `core/src/workers/gliner_relex/wire.rs`
  — `Entity` needs `text/label/start/end/score`, `TripleEntity` needs
  `text/`**`type`**`/start/end/entity_idx`. That asymmetry is real,
  upstream-imposed, and is the exact thing the hand-run smoke test caught.
  A rename here fails at the JSON-RPC boundary in production, and no
  mocked test can see it: the mocks assert the mock's own shape.
* **the relation labels asked for come back** — the model is being used
  as a joint NER+RE extractor, so a version that returns entities but no
  usable relations is a regression even though nothing raised.

Deliberately *not* asserted: that every triple's head and tail are among
the surviving entities. `GlinerModel.extract` builds `triples` by
filtering on exactly that predicate, so asserting it here cannot fail on
any input. It is already pinned, against a mock, by
`test_model.py::test_extract_filters_triples_to_surviving_entity_spans`.

How it is gated
---------------

The live test is **skip-as-pass** when the weights are not on this host,
matching the convention the Rust e2e suites use. A skip prints its
reason, so `pytest -rs` shows why.

Skip-as-pass has a known hazard: a test that skipped is indistinguishable
from a test that ran, in the result column. So there is an opt-out — set
``KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1`` and a missing weights dir
becomes a hard **failure** instead of a skip. `conftest.py` extends that
to the case this file cannot see from the inside: with the knob set, a
session in which the live test never *ran at all* — deselected by `-k`,
or renamed out of collection — fails too.

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
    REQUIRE_E2E_VAR,
    WEIGHTS_SUBPATH,
    require_live,
    weights_dir_candidate,
)

# ---------------------------------------------------------------------
# Unit tests for the pure helpers — no weights, no model, always run.
# ---------------------------------------------------------------------


def test_weights_subpath_is_the_literal_install_sh_writes():
    """Pin the constant itself, not just the joins built from it.

    Every other test here derives its expectation from `WEIGHTS_SUBPATH`,
    so a typo in the constant keeps them all green while the live test
    skips forever on every host — the exact silent-skip failure this
    file exists to abolish. The Rust `gliner_weights::WEIGHTS_SUBPATH`
    carries the identical assertion.
    """
    assert WEIGHTS_SUBPATH == "workers/gliner-relex/weights/multi-v1.0"


def test_explicit_override_is_used_verbatim():
    """`KASTELLAN_GLINER_RELEX_WEIGHTS_DIR` IS the weights dir.

    It is not a base to append `WEIGHTS_SUBPATH` to. This mirrors the
    Rust `weights_dir_candidate`, where the explicit override is the
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


def test_empty_explicit_override_falls_through_to_data_dir():
    """Set-but-empty counts as unset, at every level.

    This is where the Python and Rust halves used to disagree:
    `std::env::var` hands back `Ok("")`, so the Rust copies took the
    empty branch and built a cwd-relative path. Both now treat empty as
    absent; the pair of tests below pins each level.
    """
    env = {
        "KASTELLAN_GLINER_RELEX_WEIGHTS_DIR": "",
        "KASTELLAN_DATA_DIR": "/srv/kastellan",
    }
    assert weights_dir_candidate(env) == Path("/srv/kastellan") / WEIGHTS_SUBPATH


def test_empty_data_dir_falls_through_to_home_not_a_relative_path():
    env = {"KASTELLAN_DATA_DIR": "", "HOME": "/home/agent"}
    got = weights_dir_candidate(env)
    assert got is not None and got.is_absolute(), f"resolved to {got!r}"
    assert got == Path("/home/agent") / ".local/share/kastellan" / WEIGHTS_SUBPATH


def test_all_empty_yields_none_rather_than_a_cwd_relative_path():
    env = {
        "KASTELLAN_GLINER_RELEX_WEIGHTS_DIR": "",
        "KASTELLAN_DATA_DIR": "",
        "HOME": "",
    }
    assert weights_dir_candidate(env) is None


@pytest.mark.parametrize("value", ["1", "true", "TRUE", "yes", "on", " 1 ", "1\n", "On"])
def test_require_live_accepts_the_project_flag_dialect(value):
    """`1|true|yes|on`, trimmed and case-insensitive — the #459 dialect.

    Same set `env_flag_enabled` accepts on the Rust side. Accepting only
    `"1"` here would mean an operator who writes `=true`, the spelling
    every other kastellan flag takes, gets a silent skip from the knob
    whose whole job is to abolish silent skips. `"1\\n"` is the realistic
    one: it is what `echo "1" >> kastellan.env` produces.
    """
    assert require_live({REQUIRE_E2E_VAR: value}) is True


@pytest.mark.parametrize("value", ["0", "", "no", "off", "false", "2", "yes please"])
def test_require_live_is_off_for_everything_else(value):
    assert require_live({REQUIRE_E2E_VAR: value}) is False


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

#: Required by `wire.rs::Entity` — all five are non-`Option`, so a
#: missing one is a serde error at the JSON-RPC boundary.
ENTITY_KEYS = {"text", "label", "start", "end", "score"}

#: Required by `wire.rs::TripleEntity`. Note `type`, NOT `label`: the
#: nested head/tail dicts use a different key from the top-level
#: entities, and carry no per-position score.
TRIPLE_ENTITY_KEYS = {"text", "type", "start", "end", "entity_idx"}

#: Required by `wire.rs::Triple`.
TRIPLE_KEYS = {"head", "tail", "relation", "score"}


def _weights_dir_or_skip(env) -> Path:
    """Resolve the weights dir, or end the test the way the gate says.

    Skip (with a reason naming the path we looked at) by default; fail
    when `KASTELLAN_GLINER_RELEX_REQUIRE_E2E` demanded a real run.

    The two branches get different remedies on purpose. A dir that is
    merely absent is what `install.sh` is for; an environment where none
    of the three anchors is set cannot be fixed by running `install.sh`,
    because `install.sh` needs the same anchor to know where to write.
    """
    candidate = weights_dir_candidate(env)
    if candidate is not None and candidate.is_dir():
        return candidate

    if candidate is None:
        reason = (
            "gliner-relex weights unresolvable: none of "
            "KASTELLAN_GLINER_RELEX_WEIGHTS_DIR, KASTELLAN_DATA_DIR or HOME is set "
            "to a non-empty value — set one of them"
        )
    else:
        reason = (
            f"gliner-relex weights not found at {candidate} — "
            "run scripts/workers/gliner-relex/install.sh"
        )
    if require_live(env):
        pytest.fail(f"{REQUIRE_E2E_VAR} is set but {reason}")
    pytest.skip(reason)


def test_real_model_loads_and_extracts():
    """Load the on-disk weights for real and run one extraction.

    Device is `cpu` rather than `auto`: deterministic on both hosts, and
    it keeps the test off the GPU the daemon may be using.

    See this module's docstring for why each assertion below is the one
    chosen — in particular why the wire-key checks are the part a mocked
    test structurally cannot perform.
    """
    import os

    weights = _weights_dir_or_skip(os.environ)

    # Imported inside the test so a host without torch still collects
    # (and runs) the pure helper tests above. On a host that HAS weights,
    # an import failure here is a hard error rather than a skip, which is
    # the behaviour we want: torch is not optional once you have staged
    # the model.
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

    entities = out["entities"]
    triples = out["triples"]

    assert entities, "real model returned no entities — the load path is broken"
    assert triples, (
        "real model returned no relations — joint NER+RE is what this worker is "
        f"for, and the entities came back fine: {[e['text'] for e in entities]!r}"
    )

    # The wire contract. A rename upstream fails here rather than as a
    # serde error in `wire.rs` at dispatch time.
    for entity in entities:
        assert ENTITY_KEYS <= entity.keys(), (
            f"entity is missing wire.rs::Entity keys {ENTITY_KEYS - entity.keys()}: {entity!r}"
        )
    for triple in triples:
        assert TRIPLE_KEYS <= triple.keys(), (
            f"triple is missing wire.rs::Triple keys {TRIPLE_KEYS - triple.keys()}: {triple!r}"
        )
        for side in ("head", "tail"):
            nested = triple[side]
            assert TRIPLE_ENTITY_KEYS <= nested.keys(), (
                f"triple.{side} is missing wire.rs::TripleEntity keys "
                f"{TRIPLE_ENTITY_KEYS - nested.keys()}: {nested!r}"
            )

    # The extraction itself. Exact match, not substring: a regression
    # where NER collapses to one span covering the whole sentence would
    # satisfy `"Horst Herb" in span` while being plainly broken.
    by_text = {e["text"]: e for e in entities}
    assert "Horst Herb" in by_text, f"expected the person as its own span in {list(by_text)!r}"
    assert by_text["Horst Herb"]["label"] == "person"

    # The relation labels we asked for are the ones we got back.
    assert {t["relation"] for t in triples} <= set(LIVE_RELATION_LABELS), (
        f"model returned relations outside {LIVE_RELATION_LABELS!r}: "
        f"{sorted({t['relation'] for t in triples})!r}"
    )
    assert "maintains" in {t["relation"] for t in triples}, (
        f"the stated 'maintains' relation was not extracted: {triples!r}"
    )
