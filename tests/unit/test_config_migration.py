"""Every old field has a deliberate destination or an acknowledged retirement."""

import json
from copy import deepcopy
from pathlib import Path

import pytest
import yaml
from hypothesis import given, strategies as st
from pydantic import ValidationError

from qqbot.configuration.schema import Settings
from scripts.migrations.config_v1 import MANIFEST, MigrationError, convert, field_rules

ROOT = Path(__file__).parents[2]


def public_settings():
    return yaml.safe_load(
        (ROOT / "tests/fixtures/settings_v1.example.yaml").read_text(encoding="utf-8")
    )


def test_all_legacy_fields_are_classified():
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    assert len(manifest) == 130
    assert field_rules().keys() == manifest.keys()
    assert all(rule.reason for rule in field_rules().values())


def test_public_example_converts_to_the_operator_schema():
    result = convert(public_settings())
    assert result.settings.conversation.history_messages == 90
    assert result.settings.backends.text.model
    assert result.settings.media.max_images_per_min == 6
    assert result.settings.runtime.reply_capacity == 32
    assert "tools" not in result.sparse
    assert "diagnostics" not in result.sparse
    assert "runtime" in result.sparse


def test_sparse_values_and_reply_limit_are_preserved_without_expanding_defaults():
    raw = yaml.safe_load(
        (ROOT / "tests/fixtures/settings_v1.test.yaml").read_text(encoding="utf-8")
    )
    raw["tools"]["max_messages_per_reply"] = 5
    result = convert(raw)
    assert result.settings.conversation.max_messages_per_reply == 5
    assert result.settings.media.max_clips_per_min == 20
    assert "reply_deadline_sec" not in result.sparse["conversation"]
    assert "database" not in result.sparse.get("runtime", {})
    assert result.settings.runtime.paths.prompts_dir == "../../../config/prompts"


def test_retired_nondefault_values_require_specific_disposition():
    raw = public_settings()
    raw["diagnostics"]["error_ring_entries"] = 731
    with pytest.raises(MigrationError) as error:
        convert(raw)
    assert error.value.paths == ("diagnostics.error_ring_entries",)
    assert "731" not in str(error.value)
    result = convert(raw, retire_overrides=frozenset({"diagnostics.error_ring_entries"}))
    assert "diagnostics" not in result.sparse


@pytest.mark.parametrize("unknown", [{"surprise": {}}, {"tools": {"surprise": {}}}])
def test_unknown_empty_sections_cannot_disappear_during_conversion(unknown):
    raw = public_settings()
    raw.update(unknown)
    with pytest.raises(MigrationError, match="unknown legacy"):
        convert(raw)


def test_new_runtime_schema_does_not_dual_read_old_locations():
    raw = convert(public_settings()).sparse
    raw["tools"] = {"max_messages_per_reply": 1}
    with pytest.raises(ValidationError):
        Settings.model_validate(raw)


def test_conversion_failure_does_not_disclose_values_or_mutate_input():
    raw = public_settings()
    raw["capabilities"]["text"]["endpoint"] = "https://secret:fictional@example.invalid"
    original = deepcopy(raw)
    with pytest.raises(MigrationError) as error:
        convert(raw)
    assert "secret" not in str(error.value)
    assert "fictional" not in str(error.value)
    assert raw == original


@given(chunks=st.integers(1, 20), chunk_size=st.integers(1, 200))
def test_history_conversion_preserves_the_requested_message_count(chunks, chunk_size):
    raw = public_settings()
    raw["prompt"]["window_chunks"] = chunks
    raw["prompt"]["evict_chunk"] = chunk_size
    assert convert(raw).settings.conversation.history_messages == chunks * chunk_size
