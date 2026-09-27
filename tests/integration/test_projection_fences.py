"""Expired workers cannot checkpoint, project, age or enqueue derived state."""

from datetime import timedelta
import uuid

import pytest

import _db as _test_db
from qqbot.domain.ids import GroupId
from qqbot.domain.memory import ExtractionSnapshot
from qqbot.repositories.extraction import ExtractionRepository
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.job import JobQueue, JobType, LeaseLost, fenced_transaction
from qqbot.repositories.memory import EpisodeRepository, MemoryRepository
from qqbot.repositories.vector import VectorRepository
from qqbot.services.memory_consolidator import MemoryConsolidator
from _fixtures import now_local

pytestmark = pytest.mark.database


@pytest.fixture
async def writers(database, monkeypatch):
    return JobQueue("writer", lambda: database.pool)


async def reclaim(database, queue, job):
    await database.pool.execute(
        "UPDATE memory_job SET lease_until=clock_timestamp()-interval '1 second' WHERE id=$1",
        job.id,
    )
    return await queue.claim()


async def test_stale_checkpoint_and_projection_are_rejected_but_new_owner_can_resume(
    database, writers
):
    await writers.submit(JobType.EXTRACT_MEMORY, {"group_id": "311"})
    old = await writers.claim()
    identifier = await database.pool.fetchval(
        "INSERT INTO memory_extraction(group_id,status) VALUES (311,'extracting') RETURNING id"
    )
    repository = ExtractionRepository(database=(lambda: database.pool))
    snapshot = ExtractionSnapshot((), ())
    new = await reclaim(database, writers, old)
    with pytest.raises(LeaseLost):
        await repository.stage(identifier, snapshot, [], fence=old)
    assert (
        await database.pool.fetchval("SELECT status FROM memory_extraction WHERE id=$1", identifier)
        == "extracting"
    )
    await repository.stage(identifier, snapshot, [], fence=new)
    consolidator = MemoryConsolidator(
        object(),
        object(),
        object(),
        repository,
        writers,
        database=(lambda: database.pool),
        predicates=_test_db.test_bundle().predicates,
    )
    with pytest.raises(LeaseLost):
        await consolidator.apply(identifier, group_id=GroupId("311"), when=now_local(), fence=old)
    assert await consolidator.apply(
        identifier, group_id=GroupId("311"), when=now_local(), fence=new
    ) == (0, 0)
    assert (
        await database.pool.fetchval("SELECT status FROM memory_extraction WHERE id=$1", identifier)
        == "applied"
    )


async def test_expired_fence_blocks_each_mutation_route(database, writers):
    await writers.submit(JobType.EMBED, {"group_id": "311"})
    old = await writers.claim()
    await reclaim(database, writers, old)
    group = GroupId("311")
    operations = [
        lambda: VectorRepository("fictional", database=(lambda: database.pool)).put_episode(
            group_id=group, episode_id=uuid.uuid4(), embedding=[0], fence=old
        ),
        lambda: EpisodeRepository(database=(lambda: database.pool)).decay(
            group, ttl_days=1, fence=old
        ),
        lambda: IdentityRepository(
            database=(lambda: database.pool), clock=_test_db.clock
        ).decay_aliases(group, unused_days=1, fence=old),
        lambda: MemoryRepository(database=(lambda: database.pool), clock=_test_db.clock).decay(
            group, stable=(), fast=(), stable_days=1, default_days=1, fast_days=1, fence=old
        ),
        lambda: ExtractionRepository(database=(lambda: database.pool)).claim(
            group, limit=1, floor=1, gap=timedelta(), fence=old
        ),
        lambda: writers.submit(JobType.DECAY, {"group_id": "311"}, fence=old),
    ]
    for operation in operations:
        with pytest.raises(LeaseLost):
            await operation()
    assert await database.pool.fetchval("SELECT count(*) FROM memory_job") == 1


async def test_expiry_before_transaction_exit_rolls_back_projection(database, writers):
    await database.pool.execute("CREATE TABLE fence_probe(value integer)")
    await writers.submit(JobType.DECAY, {"group_id": "311"})
    job = await writers.claim()
    with pytest.raises(LeaseLost):
        async with fenced_transaction(lambda: database.pool, job) as conn:
            await conn.execute("INSERT INTO fence_probe VALUES (1)")
            await conn.execute(
                "UPDATE memory_job SET lease_until=clock_timestamp()-interval '1 second' "
                "WHERE id=$1",
                job.id,
            )
    assert await database.pool.fetchval("SELECT count(*) FROM fence_probe") == 0
