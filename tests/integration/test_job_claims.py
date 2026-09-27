"""PostgreSQL is the authority for claim ownership, deadlines and retry fuel."""

import asyncio
from dataclasses import replace
from datetime import timedelta
import uuid

import pytest

from qqbot.repositories.job import JobQueue, JobType, LeaseLost, require_claim

pytestmark = pytest.mark.database


def queue(database):
    return JobQueue("same-worker-id", lambda: database.pool)


async def expire(database, job):
    await database.pool.execute(
        "UPDATE memory_job SET lease_until=clock_timestamp()-interval '1 second' WHERE id=$1",
        job.id,
    )


async def test_reclaim_with_same_worker_id_rejects_every_old_claim_write(database):
    q = queue(database)
    await q.submit(JobType.EMBED, {"group_id": "311"})
    old = await q.claim()
    await expire(database, old)
    new = await q.claim()
    assert new.id == old.id and new.claim_token != old.claim_token
    assert new.retry_count == old.retry_count + 1
    assert not await q.renew(old)
    assert not await q.done(old)
    with pytest.raises(LeaseLost):
        await q.fail(old, "late failure", backoff=timedelta())
    with pytest.raises(LeaseLost):
        await q.defer(old, "late deferral", backoff=timedelta())
    async with database.pool.acquire() as conn, conn.transaction():
        with pytest.raises(LeaseLost):
            await require_claim(conn, old)
    assert await q.done(new)


async def test_expired_claim_cannot_renew_or_commit_even_before_a_reclaim(database):
    q = queue(database)
    await q.submit(JobType.DECAY, {"group_id": "311"})
    job = await q.claim()
    await expire(database, job)
    assert not await q.renew(job)
    assert not await q.done(job)
    async with database.pool.acquire() as conn, conn.transaction():
        with pytest.raises(LeaseLost):
            await require_claim(conn, job)


async def test_renewal_and_forged_token_are_distinguished(database):
    q = queue(database)
    await q.submit(JobType.DECAY, {"group_id": "311"})
    job = await q.claim()
    assert await q.renew(job, lease=timedelta(minutes=2))
    assert not await q.renew(replace(job, claim_token=uuid.uuid4()))
    assert await q.claim() is None
    assert await q.done(job)


async def test_deferral_preserves_failure_count_and_coalesces_pending_twins(database):
    q = queue(database)
    original = await q.submit(JobType.EMBED, {"group_id": "311"})
    first = await q.claim()
    await q.fail(first, "synthetic", backoff=timedelta())
    second = await q.claim()
    assert second.retry_count == 1
    await q.defer(second, "busy", backoff=timedelta())
    third = await q.claim()
    assert third.id == original and third.retry_count == 1
    twin = await q.submit(JobType.EMBED, {"group_id": "311"})
    await q.defer(third, "budget", backoff=timedelta())
    fourth = await q.claim()
    assert fourth.id == twin and fourth.retry_count == 1


async def test_expiring_claims_eventually_exhaust_the_same_job(database):
    q = queue(database)
    identifier = await q.submit(JobType.EMBED, {"group_id": "311"})
    await database.pool.execute("UPDATE memory_job SET max_retry=1 WHERE id=$1", identifier)
    first = await q.claim()
    await expire(database, first)
    second = await q.claim()
    assert second.retry_count == 1
    await expire(database, second)
    assert await q.claim() is None
    assert (await q.depth())["dead"] == 1


async def test_concurrent_claims_are_distinct_and_have_distinct_tokens(database):
    q = queue(database)
    for group in range(311, 315):
        await q.submit(JobType.EMBED, {"group_id": str(group)})
    jobs = await asyncio.gather(*(q.claim() for _ in range(4)))
    assert len({job.id for job in jobs}) == len({job.claim_token for job in jobs}) == 4
