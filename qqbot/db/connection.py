"""One runtime's lazy database pool and cancellation-safe resource lifetime."""

from __future__ import annotations

import asyncio
import json
import logging

import asyncpg

from qqbot.configuration.schema import DatabaseCfg

log = logging.getLogger("qqbot.db")


async def initialize_connection(conn: asyncpg.Connection) -> None:
    await conn.set_type_codec("jsonb", encoder=json.dumps, decoder=json.loads, schema="pg_catalog")


class Database:
    def __init__(self, settings: DatabaseCfg, *, url: str) -> None:
        self._settings = settings
        self._url = url
        self._pool: asyncpg.Pool | None = None
        self._opening: asyncio.Task | None = None
        self._closing: asyncio.Task | None = None
        self._closed = False

    def pool(self) -> asyncpg.Pool:
        if self._closed or self._pool is None:
            raise RuntimeError("database pool is not available")
        return self._pool

    async def _open(self) -> None:
        cfg = self._settings
        self._pool = await asyncpg.create_pool(
            self._url,
            min_size=cfg.pool_min,
            max_size=cfg.pool_max,
            init=initialize_connection,
            timeout=10,
            command_timeout=cfg.command_timeout_sec,
        )
        log.info("postgres pool ready")

    async def start(self) -> None:
        if self._closed:
            raise RuntimeError("database is closed")
        if self._opening is None:
            self._opening = asyncio.create_task(self._open(), name="database-open")
        await asyncio.shield(self._opening)

    async def close(self) -> None:
        if self._closing is None:
            self._closed = True
            self._closing = asyncio.create_task(self._close(), name="database-close")
        await asyncio.shield(self._closing)

    async def _close(self) -> None:
        if self._opening is not None:
            await asyncio.gather(self._opening, return_exceptions=True)
        pool, self._pool = self._pool, None
        if pool is not None:
            try:
                async with asyncio.timeout(10):
                    await pool.close()
            except BaseException:
                pool.terminate()
                raise
