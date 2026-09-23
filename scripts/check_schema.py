"""Verify that the live database matches the current canonical schema."""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from scripts._env import load_dotenv

if (ROOT / ".env").exists():
    load_dotenv(ROOT / ".env")

from qqbot.db import close_pool, init_pool
from qqbot.db.repo import ensure_schema


async def _check() -> None:
    await init_pool()
    try:
        await ensure_schema()
    finally:
        await close_pool()


def main() -> int:
    try:
        asyncio.run(_check())
    except Exception as exc:
        print(f"schema check failed: {exc}", file=sys.stderr)
        return 1
    print("database matches the canonical schema")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
