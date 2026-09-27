"""Batch fuel survives job completion, fresh job identifiers and stale ownership."""

import asyncio

import pytest

from qqbot.domain.ids import GroupId
from qqbot.domain.memory import ExtractionSnapshot
from qqbot.domain.memory.extraction import MAX_MODEL_ATTEMPTS
from qqbot.repositories.extraction import ExtractionRepository
from qqbot.repositories.job import JobQueue, JobType, LeaseLost

pytestmark = pytest.mark.database


@pytest.fixture
async def batch(database, monkeypatch):
    return await database.pool.fetchval(
        "INSERT INTO memory_extraction(group_id,status) VALUES (311,'extracting') RETURNING id"
    )


async def test_fresh_jobs_cannot_reset_a_batches_model_fuel(database, batch):
    queue = JobQueue("same-worker", lambda: database.pool)
    repo = ExtractionRepository(database=(lambda: database.pool))
    for _ in range(MAX_MODEL_ATTEMPTS):
        await queue.submit(JobType.EXTRACT_MEMORY, {"group_id": "311"})
        job = await queue.claim()
        assert await repo.begin_attempt(batch, fence=job)
        assert await queue.done(job)
    await queue.submit(JobType.EXTRACT_MEMORY, {"group_id": "311"})
    job = await queue.claim()
    assert not await repo.begin_attempt(batch, fence=job)
    row = await database.pool.fetchrow("SELECT * FROM memory_extraction WHERE id=$1", batch)
    assert row["model_attempts"] == MAX_MODEL_ATTEMPTS
    assert row["status"] == "failed"
    assert await repo.open(GroupId("311")) is None
    with pytest.raises(RuntimeError, match="exhausted"):
        await repo.stage(batch, ExtractionSnapshot((), ()), [], fence=job)


async def test_stale_owner_cannot_consume_or_terminalize_a_batch(database, batch):
    queue = JobQueue("same-worker", lambda: database.pool)
    await queue.submit(JobType.EXTRACT_MEMORY, {"group_id": "311"})
    old = await queue.claim()
    await database.pool.execute(
        "UPDATE memory_job SET lease_until=clock_timestamp()-interval '1 second' WHERE id=$1",
        old.id,
    )
    current = await queue.claim()
    repo = ExtractionRepository(database=(lambda: database.pool))
    with pytest.raises(LeaseLost):
        await repo.begin_attempt(batch, fence=old)
    assert await repo.begin_attempt(batch, fence=current)
    assert (
        await database.pool.fetchval(
            "SELECT model_attempts FROM memory_extraction WHERE id=$1", batch
        )
        == 1
    )


async def test_concurrent_reservations_cannot_overdraw_batch_fuel(database, batch):
    repo = ExtractionRepository(database=(lambda: database.pool))
    admitted = await asyncio.gather(*(repo.begin_attempt(batch) for _ in range(12)))
    assert sum(admitted) == MAX_MODEL_ATTEMPTS
    assert (
        await database.pool.fetchval(
            "SELECT model_attempts FROM memory_extraction WHERE id=$1", batch
        )
        == MAX_MODEL_ATTEMPTS
    )


async def test_staged_results_do_not_consume_more_model_attempts(database, batch):
    repo = ExtractionRepository(database=(lambda: database.pool))
    assert await repo.begin_attempt(batch)
    await repo.stage(batch, ExtractionSnapshot((), ()), [])
    assert not await repo.begin_attempt(batch)
    assert (
        await database.pool.fetchval(
            "SELECT model_attempts FROM memory_extraction WHERE id=$1", batch
        )
        == 1
    )
