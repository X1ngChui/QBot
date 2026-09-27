"""An actual PostgreSQL session lock excludes a second Runtime and detects loss."""

import asyncio
import os

import asyncpg
import pytest

from _db import _assert_connection, configure_test_database
from qqbot.db.lease import RuntimeAlreadyActive, RuntimeLease

pytestmark = pytest.mark.database


async def connect():
    conn = await asyncpg.connect(
        configure_test_database(), password=os.environ["DATABASE_PASSWORD"]
    )
    await _assert_connection(conn)
    return conn


async def test_second_runtime_is_rejected_until_the_owner_closes(database):
    key = int(database.schema[-12:], 16)
    owner = RuntimeLease(connect, key=key)
    second = RuntimeLease(connect, key=key)
    lost = asyncio.Event()
    try:
        await owner.acquire(lost.set)
        with pytest.raises(RuntimeAlreadyActive):
            await second.acquire(lost.set)
        assert not lost.is_set()
        await owner.close()
        third = RuntimeLease(connect, key=key)
        try:
            await third.acquire(lost.set)
            assert not lost.is_set()
        finally:
            await third.close()
    finally:
        await owner.close()
        await second.close()


@pytest.mark.parametrize("loss", ["connection", "unlock"])
async def test_connection_or_lock_loss_notifies_the_runtime(database, loss):
    key = int(database.schema[-12:], 16)
    lease = RuntimeLease(connect, key=key, check_interval=0.01)
    lost = asyncio.Event()
    try:
        await lease.acquire(lost.set)
        if loss == "connection":
            lease._connection.terminate()
        else:
            await lease._connection.execute("SELECT pg_advisory_unlock($1::bigint)", key)
        await asyncio.wait_for(lost.wait(), 1)
    finally:
        await lease.close()
