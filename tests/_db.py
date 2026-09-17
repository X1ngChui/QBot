"""Isolated database setup shared by destructive tests and paid evals.

The target has a distinct role, database and marker schema. Ordinary production
connection variables are never inherited.
"""

from __future__ import annotations

import os
from pathlib import Path
from urllib.parse import urlparse


DEFAULT_TEST_DATABASE_URL = "postgresql://qbot_test@127.0.0.1:15432/qbot_test"
DEFAULT_TEST_DATABASE_PASSWORD = "testpw"
_ALLOWED_DATABASE = "qbot_test"
_ALLOWED_USER = "qbot_test"
_MARKER = "qbot-disposable-v1"
_SCHEMA = Path(__file__).resolve().parent.parent / "sql" / "init.sql"


def configure_test_database() -> str:
    """Install the explicit test DSN before importing database or settings modules."""
    url = os.environ.get("QBOT_TEST_DATABASE_URL", DEFAULT_TEST_DATABASE_URL)
    parsed = urlparse(url)
    if (parsed.scheme not in ("postgresql", "postgres")
            or parsed.path.lstrip("/") != _ALLOWED_DATABASE
            or parsed.username != _ALLOWED_USER):
        raise RuntimeError(
            "QBOT_TEST_DATABASE_URL must name database and user qbot_test; "
            "initialize it with tests/fixtures/test_db_marker.sql"
        )
    os.environ["DATABASE_URL"] = url
    os.environ.pop("DATABASE_PASSWORD_FILE", None)
    os.environ["DATABASE_PASSWORD"] = os.environ.get(
        "QBOT_TEST_DATABASE_PASSWORD", DEFAULT_TEST_DATABASE_PASSWORD
    )
    return url


async def _assert_connection(conn) -> None:
    identity = await conn.fetchrow(
        "SELECT current_database() AS database, current_user AS username"
    )
    actual = dict(identity)
    if (actual["database"] != _ALLOWED_DATABASE
            or actual["username"] != _ALLOWED_USER):
        raise RuntimeError(
            "refusing destructive access to a non-test database: "
            f"database={actual['database']!r} user={actual['username']!r}"
        )
    marker_table = await conn.fetchval(
        "SELECT to_regclass('qbot_test_guard.identity')::text"
    )
    if marker_table != "qbot_test_guard.identity":
        raise RuntimeError("refusing destructive access: disposable database marker is absent")
    marker = await conn.fetchval(
        "SELECT marker FROM qbot_test_guard.identity WHERE marker=$1", _MARKER
    )
    if marker != _MARKER:
        raise RuntimeError("refusing destructive access: disposable database marker is invalid")


async def assert_disposable_database() -> None:
    """Verify the live database identity and immutable marker."""
    from qqbot.db import pool

    async with pool().acquire() as conn:
        await _assert_connection(conn)


async def reset() -> None:
    """Truncate public tables after checking the same connection in one transaction."""
    from qqbot.db import pool

    async with pool().acquire() as conn, conn.transaction():
        await _assert_connection(conn)
        # Keep a long-lived disposable container aligned with the checked-in schema.
        # This runs only after the database/user/marker guard above; production can
        # never reach it through an inherited DATABASE_URL.
        await conn.execute(_SCHEMA.read_text(encoding="utf-8"))
        rows = await conn.fetch(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public'"
        )
        names = [r["tablename"] for r in rows]
        if names:
            await conn.execute("TRUNCATE " + ", ".join(names))
