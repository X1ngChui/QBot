"""Public references and examples follow the active schema, not private settings."""

from pathlib import Path

import yaml

from qqbot.configuration import Settings
from qqbot.configuration.schema import Settings as ActiveSettings
from scripts.config_reference import migration_table, reference, schema_fields
from scripts.migrations.config_v1 import convert

ROOT = Path(__file__).parents[2]


def test_active_loader_and_reference_use_the_same_schema():
    assert Settings is ActiveSettings
    assert set(Settings.model_fields) == {
        "bot",
        "conversation",
        "backends",
        "media",
        "memory",
        "budget",
        "tasks",
        "maintenance",
        "runtime",
    }
    fields = schema_fields()
    assert len({path for path, _ in fields}) == len(fields)
    assert all(spec.get("description") for _, spec in fields)


def test_checked_in_references_are_current():
    assert (ROOT / "docs/configuration-reference.md").read_text(encoding="utf-8") == reference()
    assert (ROOT / "docs/configuration-migration.md").read_text(
        encoding="utf-8"
    ) == migration_table()


def test_simplified_example_preserves_all_retained_public_choices():
    previous = yaml.safe_load(
        (ROOT / "tests/fixtures/settings_v1.example.yaml").read_text(encoding="utf-8")
    )
    current = yaml.safe_load((ROOT / "config/settings.yaml.example").read_text(encoding="utf-8"))
    assert Settings.model_validate(current) == convert(previous).settings
