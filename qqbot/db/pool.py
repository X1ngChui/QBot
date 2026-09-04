"""asyncpg connection pool (section 2: pool size 2-8)."""

from __future__ import annotations

import json
import logging
import os
from urllib.parse import quote

import asyncpg

from ..util import read_secret

log = logging.getLogger("qqbot.db")

_pool: asyncpg.Pool | None = None


def dsn(*, with_password: bool = True) -> str:
    """Full DSN with the secret password injected. Also used by the backup job -
    which passes with_password=False and carries the secret in PGPASSWORD instead,
    keeping it out of pg_dump's argv (readable in /proc for the dump's duration)."""
    url = os.getenv("DATABASE_URL", "postgresql://qqbot@postgres:5432/qqbot")
    if not with_password:
        return url
    pwd = read_secret("DATABASE_PASSWORD_FILE", "DATABASE_PASSWORD")
    if pwd and "@" in url and ":" not in url.split("//", 1)[1].split("@", 1)[0]:
        head, tail = url.split("//", 1)
        user, rest = tail.split("@", 1)
        url = f"{head}//{user}:{quote(pwd, safe='')}@{rest}"
    return url


async def _init_conn(conn: asyncpg.Connection) -> None:
    await conn.set_type_codec(
        "jsonb", encoder=json.dumps, decoder=json.loads, schema="pg_catalog"
    )


async def init_pool() -> asyncpg.Pool:
    global _pool
    if _pool is None:
        _pool = await asyncpg.create_pool(
            dsn(), min_size=2, max_size=8, init=_init_conn, command_timeout=20
        )
        log.info("postgres pool ready")
    return _pool


async def close_pool() -> None:
    global _pool
    if _pool is not None:
        await _pool.close()
        _pool = None


def pool() -> asyncpg.Pool:
    if _pool is None:
        raise RuntimeError("connection pool is not initialized")
    return _pool
