"""Round-trip migration preserves human edits and never exposes values in reports."""

from pathlib import Path

import pytest
from ruamel.yaml import YAML

from scripts.migrate_config import migrate_file, render_migration
from scripts.migrations.config_v1 import MigrationError

ROOT = Path(__file__).parents[2]


def example():
    text = (ROOT / "tests/fixtures/settings_v1.example.yaml").read_text(encoding="utf-8")
    text = "# Operator note retained.\n" + text
    text = text.replace(
        "endpoint: https://api.deepseek.com",
        'endpoint: "https://api.deepseek.com" # Keep this connection note.',
        1,
    )
    return text


def test_round_trip_preserves_header_inline_comments_and_quoted_values():
    rendered, result = render_migration(example())
    assert "# Operator note retained." in rendered
    assert "# Keep this connection note." in rendered
    assert 'endpoint: "https://api.deepseek.com"' in rendered
    assert result.settings.bot.owners == ("10001",)
    assert "backends:" in rendered and "capabilities:" not in rendered


def test_dry_run_leaves_the_input_and_backup_directory_untouched(tmp_path):
    source = tmp_path / "settings.yaml"
    source.write_text(example(), encoding="utf-8")
    before = source.read_bytes()
    backup_dir = tmp_path / "backups"
    migrate_file(source, backup_dir=backup_dir)
    assert source.read_bytes() == before
    assert not backup_dir.exists()


def test_apply_creates_recoverable_backup_outside_the_configuration_tree(tmp_path):
    source = tmp_path / "config" / "settings.yaml"
    source.parent.mkdir()
    bom = bytes.fromhex("efbbbf")
    before = bom + example().replace("\n", "\r\n").encode("utf-8")
    source.write_bytes(before)
    backup_dir = tmp_path / "backups"
    result = migrate_file(source, apply=True, backup_dir=backup_dir)
    assert list(source.parent.iterdir()) == [source]
    (backup,) = backup_dir.iterdir()
    assert backup.read_bytes() == before
    after = source.read_bytes()
    assert after.startswith(bom) and b"\r\n" in after
    assert result.settings.bot.owners == ("10001",)
    assert "backends" in YAML().load(after.decode("utf-8-sig"))
    with pytest.raises(MigrationError):
        migrate_file(source, apply=True, backup_dir=backup_dir)
    assert source.read_bytes() == after
    assert list(backup_dir.iterdir()) == [backup]
