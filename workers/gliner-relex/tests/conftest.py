"""Shared pytest fixtures.

The mocked-model fixture lets us exercise server.py + model.py contract
without loading 1.3 GB of weights.

The real-model load — which no mocked test can prove still works after a
library upgrade — is covered by `test_model_live.py`, which loads the
on-disk weights for real and skips-as-pass when they are absent (set
`KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1` to make that a failure instead).
The Rust side's `core/tests/entity_extraction_e2e.rs` covers the same
path through the full sandboxed worker.
"""
from unittest.mock import MagicMock

import pytest


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
