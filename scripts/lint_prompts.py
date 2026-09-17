"""Validate the complete prompt bundle without model calls or database access."""

from __future__ import annotations

import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "config"))

from qqbot.prompting.lint import lint_catalog
from qqbot.settings import load_bundle


def main() -> int:
    bundle = load_bundle(ROOT / "config")
    errors = lint_catalog(bundle.prompts, bundle.default)
    if errors:
        for error in errors:
            print(f"ERROR: {error}")
        return 1
    print(f"prompt bundle valid: {len(bundle.prompts.templates)} templates")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
