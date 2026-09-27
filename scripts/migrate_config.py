"""Comment-preserving, explicit offline configuration migration entry point."""

from __future__ import annotations

import argparse
from copy import deepcopy
import hashlib
from io import StringIO
import os
from pathlib import Path
import stat
import sys
import tempfile

from ruamel.yaml import YAML
from ruamel.yaml.comments import CommentedMap

ROOT = Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from qqbot.configuration.schema import Settings
from scripts.migrations.config_v1 import MigrationError, MigrationResult, convert


def _parent(mapping, path: str):
    parts = path.split(".")
    node = mapping
    for part in parts[:-1]:
        node = node[part]
    return node, parts[-1]


def _target(mapping: CommentedMap, path: str):
    parts = path.split(".")
    node = mapping
    for part in parts[:-1]:
        if part not in node:
            node[part] = CommentedMap()
        node = node[part]
    return node, parts[-1]


def render_migration(
    text: str, *, retire_overrides: frozenset[str] = frozenset()
) -> tuple[str, MigrationResult]:
    yaml = YAML()
    yaml.preserve_quotes = True
    yaml.allow_duplicate_keys = False
    yaml.width = 100
    yaml.indent(mapping=2, sequence=4, offset=2)
    # ruamel.yaml infers None here, but its emitter accepts a string line ending.
    yaml.line_break = "\r\n" if "\r\n" in text else "\n"  # pyright: ignore[reportAttributeAccessIssue]
    original = yaml.load(text)
    if not isinstance(original, CommentedMap):
        raise MigrationError(["<root>"], "expected a YAML mapping")
    result = convert(original, retire_overrides=retire_overrides)
    target = CommentedMap()
    target.ca.comment = deepcopy(original.ca.comment)
    for name in Settings.model_fields:
        if name in result.sparse:
            target[name] = CommentedMap()
    written = set()
    for disposition in result.fields:
        if not disposition.explicit or disposition.target is None:
            continue
        destination = disposition.target
        if destination in written:
            continue
        written.add(destination)
        old_parent, old_key = _parent(original, disposition.source)
        new_parent, new_key = _target(target, destination)
        if disposition.derived:
            sparse_parent, sparse_key = _parent(result.sparse, destination)
            new_parent[new_key] = sparse_parent[sparse_key]
        else:
            new_parent[new_key] = deepcopy(old_parent[old_key])
        if old_key in old_parent.ca.items:
            new_parent.ca.items[new_key] = deepcopy(old_parent.ca.items[old_key])
        if old_parent is not original and new_parent.ca.comment is None:
            new_parent.ca.comment = deepcopy(old_parent.ca.comment)
    output = StringIO()
    yaml.dump(target, output)
    rendered = output.getvalue()
    if Settings.model_validate(yaml.load(rendered)) != result.settings:
        raise RuntimeError("round-trip migration changed the validated configuration")
    return rendered, result


def migrate_file(
    path: Path,
    *,
    apply: bool = False,
    retire_overrides: frozenset[str] = frozenset(),
    backup_dir: Path | None = None,
) -> MigrationResult:
    before = path.read_bytes()
    rendered, result = render_migration(
        before.decode("utf-8-sig"), retire_overrides=retire_overrides
    )
    if not apply:
        return result
    bom = bytes.fromhex("efbbbf")
    encoded = (bom if before.startswith(bom) else b"") + rendered.encode("utf-8")
    fingerprint = hashlib.sha256(before).hexdigest()[:12]
    backup_dir = backup_dir or ROOT / "backups" / "config-migrations"
    backup_dir.mkdir(parents=True, exist_ok=True)
    backup = backup_dir / (path.name + ".before-refactor-" + fingerprint)
    if backup.exists():
        if backup.read_bytes() != before:
            raise RuntimeError("the migration backup exists with different contents")
    else:
        with backup.open("xb") as stream:
            stream.write(before)
            stream.flush()
            os.fsync(stream.fileno())
    mode = stat.S_IMODE(path.stat().st_mode)
    descriptor, name = tempfile.mkstemp(prefix=path.name + ".migrating-", dir=path.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        temporary.chmod(mode)
        if path.read_bytes() != before:
            raise RuntimeError("configuration changed during migration; no replacement was made")
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    parser.add_argument(
        "--apply", action="store_true", help="Back up and atomically replace the file."
    )
    parser.add_argument(
        "--retire",
        action="append",
        default=[],
        metavar="OLD.PATH",
        help="Acknowledge one retired nondefault implementation setting.",
    )
    args = parser.parse_args()
    try:
        result = migrate_file(args.path, apply=args.apply, retire_overrides=frozenset(args.retire))
    except MigrationError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    except Exception as exc:
        print(
            f"migration failed ({type(exc).__name__}); configuration values are not displayed",
            file=sys.stderr,
        )
        return 1
    for field in result.fields:
        if field.explicit:
            action = field.target or "retired"
            print(f"{field.source} -> {action}")
    print("Migration applied with a backup." if args.apply else "Dry-run valid; no files changed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
