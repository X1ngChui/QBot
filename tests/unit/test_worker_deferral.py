"""Deferred work keeps the same durable job rather than creating a fresh retry chain."""

from datetime import timedelta
from types import SimpleNamespace
from unittest.mock import AsyncMock
import uuid

from qqbot.repositories.job import Job, JobType, LeaseLost
from qqbot.workers.lease import Deferred
from qqbot.workers.memory import MemoryWorker


def worker(error):
    job = Job(uuid.uuid4(), JobType.EMBED, {"group_id": "311"}, 2, 5, uuid.uuid4())
    instance = MemoryWorker.__new__(MemoryWorker)
    instance._queue = SimpleNamespace(
        claim=AsyncMock(return_value=job),
        renew=AsyncMock(return_value=True),
        done=AsyncMock(return_value=True),
        fail=AsyncMock(),
        defer=AsyncMock(),
    )
    instance._retry_backoff = (timedelta(seconds=1),)
    instance._dispatch = AsyncMock(side_effect=error)
    return instance, job


async def test_busy_or_budget_deferral_preserves_the_original_claim():
    instance, job = worker(Deferred("busy", delay=timedelta(seconds=15)))
    assert await instance.step()
    instance._queue.defer.assert_awaited_once_with(job, "busy", backoff=timedelta(seconds=15))
    instance._queue.done.assert_not_awaited()
    instance._queue.fail.assert_not_awaited()


async def test_lost_owner_never_reports_done_or_spends_retry_fuel():
    instance, _ = worker(LeaseLost("synthetic loss"))
    assert await instance.step()
    instance._queue.done.assert_not_awaited()
    instance._queue.fail.assert_not_awaited()
    instance._queue.defer.assert_not_awaited()


async def test_failure_is_charged_to_the_same_claim():
    instance, job = worker(RuntimeError("synthetic failure"))
    assert await instance.step()
    assert instance._queue.fail.call_args.args[0] is job
    instance._queue.done.assert_not_awaited()


async def test_provider_budget_stop_defers_without_resetting_failure_fuel():
    from qqbot.services.budget import BudgetUnavailable

    instance, job = worker(BudgetUnavailable("ledger unavailable after waiting for provider"))
    assert await instance.step()
    assert instance._queue.defer.call_args.args[0] is job
    instance._queue.done.assert_not_awaited()
    instance._queue.fail.assert_not_awaited()
