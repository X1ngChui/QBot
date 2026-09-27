"""An owned pool cannot be closed by another runtime or abandoned by a cancelled waiter."""

import asyncio
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock

import pytest

from qqbot.configuration import DatabaseCfg
from qqbot.db import connection
from qqbot.db.connection import Database


@pytest.fixture
def pool(monkeypatch):
    value = SimpleNamespace(close=AsyncMock(), terminate=Mock())
    create = AsyncMock(return_value=value)
    monkeypatch.setattr(connection.asyncpg, "create_pool", create)
    return value, create


async def test_database_uses_explicit_settings_and_one_initialization(pool):
    owned, create = pool
    owner = Database(DatabaseCfg(pool_min=1, pool_max=3, command_timeout_sec=7), url="fictional")
    await asyncio.gather(owner.start(), owner.start())
    assert owner.pool() is owned
    create.assert_awaited_once()
    assert create.call_args.kwargs["min_size"] == 1
    assert create.call_args.kwargs["max_size"] == 3
    assert create.call_args.kwargs["command_timeout"] == 7
    await asyncio.gather(owner.close(), owner.close())
    owned.close.assert_awaited_once()
    with pytest.raises(RuntimeError, match="available"):
        owner.pool()
    with pytest.raises(RuntimeError, match="closed"):
        await owner.start()


async def test_unstarted_database_cannot_close_another_instances_pool(pool):
    owned, create = pool
    first = Database(DatabaseCfg(), url="fictional-first")
    second = Database(DatabaseCfg(), url="fictional-second")
    await first.start()
    await second.close()
    owned.close.assert_not_awaited()
    assert first.pool() is owned
    await first.close()


async def test_cancelled_start_waiter_does_not_lose_a_late_pool(pool):
    owned, create = pool
    entered, release = asyncio.Event(), asyncio.Event()

    async def open_pool(*args, **kwargs):
        entered.set()
        await release.wait()
        return owned

    create.side_effect = open_pool
    owner = Database(DatabaseCfg(), url="fictional")
    start = asyncio.create_task(owner.start())
    await entered.wait()
    start.cancel()
    with pytest.raises(asyncio.CancelledError):
        await start
    closing = asyncio.create_task(owner.close())
    await asyncio.sleep(0)
    assert not closing.done()
    release.set()
    await closing
    owned.close.assert_awaited_once()


async def test_close_waiter_cancellation_does_not_abandon_pool_cleanup(pool):
    owned, _ = pool
    entered, release = asyncio.Event(), asyncio.Event()

    async def close():
        entered.set()
        await release.wait()

    owned.close.side_effect = close
    owner = Database(DatabaseCfg(), url="fictional")
    await owner.start()
    closing = asyncio.create_task(owner.close())
    await entered.wait()
    closing.cancel()
    with pytest.raises(asyncio.CancelledError):
        await closing
    release.set()
    await owner.close()
    owned.close.assert_awaited_once()


async def test_failed_pool_shutdown_terminates_the_owned_connections(pool):
    owned, _ = pool
    owned.close.side_effect = OSError("synthetic shutdown failure")
    owner = Database(DatabaseCfg(), url="fictional")
    await owner.start()
    with pytest.raises(OSError):
        await owner.close()
    owned.terminate.assert_called_once()
