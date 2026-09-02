"""Shared pytest fixtures, and the session-level guard on the live test.

The mocked-model fixture lets us exercise server.py + model.py contract
without loading 1.3 GB of weights.

The real-model load — which no mocked test can prove still works after a
library upgrade — is covered by `test_model_live.py`, which loads the
on-disk weights for real and skips-as-pass when they are absent. Setting
`KASTELLAN_GLINER_RELEX_REQUIRE_E2E` (`1`/`true`/`yes`/`on`) turns that
skip into a failure, and the `pytest_sessionfinish` hook below turns the
cases the test cannot see from the inside into failures too.

The same round-trip through the full sandboxed worker lives on the Rust
side: `core/tests/gliner_relex_e2e.rs` and `--test entity_extraction_e2e`
(both skip-as-pass without venv + weights; both additionally need
`KASTELLAN_GLINER_RELEX_ENABLE=1`).
"""
import os
from unittest.mock import MagicMock

import pytest

from tests.live_support import REQUIRE_E2E_VAR, require_live

#: The test whose having-actually-run is what a green run under
#: `KASTELLAN_GLINER_RELEX_REQUIRE_E2E` is supposed to mean. Matched by
#: name rather than full node id so moving the file does not silently
#: disarm the guard.
LIVE_TEST_NAME = "test_real_model_loads_and_extracts"

_live_outcome = {"collected": False, "passed": False}


@pytest.hookimpl(trylast=True)
def pytest_collection_modifyitems(items):
    """Record whether the live test survived collection AND selection.

    `trylast` is load-bearing: `-k` / `-m` deselection is itself done by a
    `pytest_collection_modifyitems` impl, so a hook that runs first sees
    the pre-filter list and would report a deselected test as present.
    Running last means `items` is the final selection — which is exactly
    the case the in-test knob cannot detect.
    """
    _live_outcome["collected"] = any(
        item.name.split("[")[0] == LIVE_TEST_NAME for item in items
    )


def pytest_runtest_logreport(report):
    if report.when == "call" and report.nodeid.split("::")[-1].split("[")[0] == LIVE_TEST_NAME:
        _live_outcome["passed"] = report.passed


def pytest_sessionfinish(session, exitstatus):
    """Fail a `REQUIRE_E2E` session in which the live test never ran.

    The in-test knob only fires from inside the test body, so it closes
    exactly one hole: missing weights. It is blind to the two that leave
    no trace — the test being deselected (`-k "not live"`, `--deselect`,
    `--ignore`) and the test being renamed off the `test_` prefix or out
    of `python_files`. Both exit 0 with the knob set and zero model
    loads, which is the same class of false green (`51 passed` vs
    `50 passed, 1 deselected` differ by one integer a human must notice)
    that this whole file exists to abolish.

    Only tightens: a session that legitimately ran the live test, or one
    without the knob set, is untouched.
    """
    if not require_live(os.environ) or _live_outcome["passed"]:
        return

    if not _live_outcome["collected"]:
        detail = (
            f"{LIVE_TEST_NAME} was not collected — it was deselected, ignored, or "
            "renamed out of collection"
        )
    else:
        detail = f"{LIVE_TEST_NAME} was collected but did not pass"

    reporter = session.config.pluginmanager.get_plugin("terminalreporter")
    if reporter is not None:
        reporter.write_sep("=", "REQUIRE_E2E guard", red=True)
        reporter.write_line(
            f"{REQUIRE_E2E_VAR} demanded a real model run, but {detail}. "
            "A green result here would mean nothing."
        )
    session.exitstatus = 1


@pytest.fixture
def fake_model():
    """A minimal stand-in for the loaded GLiNER object.

    Returns canned (entities, triples) regardless of input — enough for
    server.py's dispatch path tests. test_model.py exercises the real
    GLiNER wrapper separately with its own MagicMock that returns
    fine-grained per-call values.

    Triple shape uses `head` / `tail` carrying full Entity dicts inline,
    matching upstream `model.inference(...)` envelope (see spike notes
    correction #2). Consumers can read head.label / head.start without
    a second lookup.
    """
    smith = {"text": "Smith", "label": "person", "start": 0, "end": 5, "score": 0.91}
    asthma = {"text": "asthma", "label": "disease", "start": 13, "end": 19, "score": 0.88}
    m = MagicMock(name="FakeGliNER")
    m.extract.return_value = {
        "entities": [smith, asthma],
        "triples": [
            {"head": smith, "tail": asthma, "relation": "treats", "score": 0.77},
        ],
    }
    return m
