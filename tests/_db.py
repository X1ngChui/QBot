"""Isolated database setup shared by destructive tests and paid evals.

The target has a distinct role, database and marker schema. Ordinary production
connection variables are never inherited.
"""

from __future__ import annotations

import os
from urllib.parse import urlparse


DEFAULT_TEST_DATABASE_URL = "postgresql://qbot_test@127.0.0.1:15432/qbot_test"
DEFAULT_TEST_DATABASE_PASSWORD = "testpw"
_ALLOWED_DATABASE = "qbot_test"
_ALLOWED_USER = "qbot_test"
_MARKER = "qbot-disposable-v1"


def configure_test_database() -> str:
    """Install the explicit test DSN before importing database or settings modules."""
    url = os.environ.get("QBOT_TEST_DATABASE_URL", DEFAULT_TEST_DATABASE_URL)
    parsed = urlparse(url)
    if (
        parsed.scheme not in ("postgresql", "postgres")
        or parsed.path.lstrip("/") != _ALLOWED_DATABASE
        or parsed.username != _ALLOWED_USER
    ):
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
    if actual["database"] != _ALLOWED_DATABASE or actual["username"] != _ALLOWED_USER:
        raise RuntimeError(
            "refusing destructive access to a non-test database: "
            f"database={actual['database']!r} user={actual['username']!r}"
        )
    marker_table = await conn.fetchval("SELECT to_regclass('qbot_test_guard.identity')::text")
    if marker_table != "qbot_test_guard.identity":
        raise RuntimeError("refusing destructive access: disposable database marker is absent")
    marker = await conn.fetchval(
        "SELECT marker FROM qbot_test_guard.identity WHERE marker=$1", _MARKER
    )
    if marker != _MARKER:
        raise RuntimeError("refusing destructive access: disposable database marker is invalid")


async def assert_disposable_database(database=None) -> None:
    """Verify the live database identity and immutable marker."""

    async with (database or pool)().acquire() as conn:
        await _assert_connection(conn)


async def reset(database=None) -> None:
    """Truncate canonical tables after checking the same guarded connection."""
    from qqbot.db.repo import check_schema
    from qqbot.domain.memory.embedding import VECTOR_DIMENSIONS

    async with (database or pool)().acquire() as conn, conn.transaction():
        await _assert_connection(conn)
        dimensions = VECTOR_DIMENSIONS
        await check_schema(conn, schema="public", embedding_dimensions=dimensions)
        rows = await conn.fetch("SELECT tablename FROM pg_tables WHERE schemaname = 'public'")
        names = [row["tablename"] for row in rows]
        if names:
            await conn.execute("TRUNCATE " + ", ".join(names))


_owner = None


async def init_pool():
    global _owner
    from _fixtures import config
    from qqbot.db import Database, dsn

    if _owner is None:
        configure_test_database()
        _owner = Database(config().default.runtime.database, url=dsn())
        try:
            await _owner.start()
            await assert_disposable_database()
        except BaseException:
            await close_pool()
            raise
    return _owner.pool()


def pool():
    if _owner is None:
        raise RuntimeError("test database is not initialized")
    return _owner.pool()


async def close_pool():
    global _owner
    owner, _owner = _owner, None
    if owner is not None:
        await owner.close()


from _fixtures import clock

from qqbot.repositories.groups import GroupRepository
from qqbot.repositories.media_cache import MediaCacheRepository
from qqbot.repositories.evidence import EvidenceRepository
from qqbot.repositories.archive import ArchiveRepository
from qqbot.repositories.identity import IdentityRepository

groups = GroupRepository(pool, clock=clock)
media_cache = MediaCacheRepository(pool, clock=clock)
evidence = EvidenceRepository(pool)
archive = ArchiveRepository(database=pool)
identities = IdentityRepository(database=pool, clock=clock)


def test_bundle():
    from _fixtures import config

    return config()


def bundle_for_settings(settings):
    from qqbot.configuration import ConfigBundle

    base = test_bundle()
    return ConfigBundle(
        settings.model_dump(),
        {"default": base._default_persona, **base.personas},
        base.prompts,
        base.predicates,
    )


def fact_card(*args, **kwargs):
    from dataclasses import replace
    from qqbot.services.directory import FactCard
    from qqbot.services.context_builder import render_fact

    card = FactCard(*args, **kwargs)
    return replace(
        card,
        rendered=render_fact(
            card.predicate, card.object, card.object_key, predicates=test_bundle().predicates
        ),
    )


def tool_registry(settings):
    from qqbot.conversation.tools import tool_registry as build

    return build(settings, prompts=test_bundle().prompts)
