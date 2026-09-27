"""A dedicated PostgreSQL session proves exclusive ownership of one live Runtime."""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable

import asyncpg


class RuntimeAlreadyActive(RuntimeError):
    pass


class RuntimeLease:
    def __init__(
        self,
        connect: Callable[[], Awaitable[asyncpg.Connection]],
        *,
        key: int = 0x51424F54,
        check_interval: float = 5,
    ) -> None:
        self._connect = connect
        self._key = key
        self._interval = check_interval
        self._connection: asyncpg.Connection | None = None
        self._watcher: asyncio.Task | None = None
        self._closed = False
        self._lost = False
        self._on_lost: Callable[[], None] | None = None

    async def acquire(self, on_lost: Callable[[], None]) -> None:
        if self._closed or self._connection is not None:
            raise RuntimeError("runtime lease cannot be reused")
        self._on_lost = on_lost
        conn = await self._connect()
        self._connection = conn
        try:
            if not await conn.fetchval("SELECT pg_try_advisory_lock($1::bigint)", self._key):
                raise RuntimeAlreadyActive("another Runtime already owns this database")
            conn.add_termination_listener(self._terminated)
            self._watcher = asyncio.create_task(self._watch(conn), name="runtime-lease")
        except BaseException:
            await self.close()
            raise

    def _terminated(self, connection) -> None:
        if connection is self._connection:
            self._notify_loss()

    def _notify_loss(self) -> None:
        if not self._closed and not self._lost:
            self._lost = True
            if self._on_lost is not None:
                self._on_lost()

    async def _watch(self, conn: asyncpg.Connection) -> None:
        try:
            while not self._closed:
                await asyncio.sleep(self._interval)
                held = await conn.fetchval(
                    """SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype='advisory'
                       AND pid=pg_backend_pid() AND granted AND objsubid=1
                       AND classid=(($1::bigint >> 32) & 4294967295)::oid
                       AND objid=($1::bigint & 4294967295)::oid)""",
                    self._key,
                )
                if not held:
                    self._notify_loss()
                    return
        except asyncio.CancelledError:
            raise
        except Exception:
            self._notify_loss()

    async def close(self) -> None:
        self._closed = True
        watcher, self._watcher = self._watcher, None
        if watcher is not None:
            watcher.cancel()
            await asyncio.gather(watcher, return_exceptions=True)
        connection, self._connection = self._connection, None
        if connection is not None:
            await connection.close(timeout=5)
