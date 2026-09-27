"""Lease renewal failure stops the owned execution without leaking sibling tasks."""

import asyncio
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from qqbot.repositories.job import LeaseLost
from qqbot.workers.lease import ClaimLease


@pytest.mark.parametrize("failure", [False, RuntimeError("synthetic disconnect")])
async def test_lost_renewal_cancels_and_joins_execution(failure):
    entered, cancelled, tick = asyncio.Event(), asyncio.Event(), asyncio.Event()
    queue = SimpleNamespace(renew=AsyncMock(side_effect=[True, failure]))

    async def execute():
        entered.set()
        try:
            await asyncio.Event().wait()
        finally:
            cancelled.set()

    lease = ClaimLease(queue, object(), pause=lambda _: tick.wait())
    running = asyncio.create_task(lease.run(execute))
    await entered.wait()
    tick.set()
    with pytest.raises(LeaseLost):
        await running
    assert cancelled.is_set() and queue.renew.await_count == 2


async def test_external_cancellation_joins_execution_and_heartbeat():
    entered, cancelled, heartbeat_cancelled = asyncio.Event(), asyncio.Event(), asyncio.Event()
    queue = SimpleNamespace(renew=AsyncMock(return_value=True))

    async def pause(_):
        try:
            await asyncio.Event().wait()
        finally:
            heartbeat_cancelled.set()

    async def execute():
        entered.set()
        try:
            await asyncio.Event().wait()
        finally:
            cancelled.set()

    running = asyncio.create_task(ClaimLease(queue, object(), pause=pause).run(execute))
    await entered.wait()
    running.cancel()
    with pytest.raises(asyncio.CancelledError):
        await running
    assert cancelled.is_set() and heartbeat_cancelled.is_set()


async def test_already_lost_lease_does_not_start_execution():
    execute = AsyncMock()
    queue = SimpleNamespace(renew=AsyncMock(return_value=False))
    with pytest.raises(LeaseLost):
        await ClaimLease(queue, object()).run(execute)
    execute.assert_not_called()
