"""Durable bounded-retry work with per-claim fencing, renewal and deferral."""

from __future__ import annotations

import uuid
from collections.abc import AsyncIterator, Callable
from contextlib import asynccontextmanager
from dataclasses import dataclass
from datetime import timedelta
from enum import StrEnum

import asyncpg


class JobType(StrEnum):
    EXTRACT_MEMORY = "extract_memory"
    EMBED = "embed"
    DECAY = "decay"


class LeaseLost(RuntimeError):
    """This execution no longer has permission to publish durable results."""


@dataclass(frozen=True, slots=True)
class Job:
    id: uuid.UUID
    job_type: JobType
    payload: dict
    retry_count: int
    max_retry: int
    claim_token: uuid.UUID

    @property
    def exhausted(self) -> bool:
        return self.retry_count >= self.max_retry


async def require_claim(conn: asyncpg.Connection, job: Job) -> None:
    """Validate after acquiring the row lock, not before an unbounded lock wait."""
    if not conn.is_in_transaction():
        raise RuntimeError("claim checks require the side-effect transaction")
    await conn.fetchval("SELECT id FROM memory_job WHERE id=$1 FOR UPDATE", job.id)
    owned = await conn.fetchval(
        """SELECT id FROM memory_job WHERE id=$1 AND claim_token=$2
           AND status='running' AND lease_until > clock_timestamp()""",
        job.id,
        job.claim_token,
    )
    if owned is None:
        raise LeaseLost(f"job {job.id} no longer owns its claim")


@asynccontextmanager
async def fenced_transaction(
    database: Callable[[], asyncpg.Pool],
    fence: Job | None,
) -> AsyncIterator[asyncpg.Connection]:
    """Fence leased background writes; direct administrative writes have no lease."""
    async with database().acquire() as conn, conn.transaction():
        if fence is not None:
            await require_claim(conn, fence)
        yield conn
        if fence is not None:
            await require_claim(conn, fence)


class JobQueue:
    LEASE = timedelta(seconds=60)

    def __init__(self, worker_id: str, database: Callable[[], asyncpg.Pool]) -> None:
        self._worker = worker_id
        self._database = database

    @staticmethod
    async def _group_lock(conn, job_type: JobType, payload: dict) -> None:
        # Producers and retry/deferral coalescing serialize on the deduplication key.
        await conn.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            f"job-submit:{job_type.value}:{payload.get('group_id', '')}",
        )

    async def submit(
        self,
        job_type: JobType,
        payload: dict,
        *,
        priority: int = 0,
        delay: timedelta | None = None,
        fence: Job | None = None,
        _conn: asyncpg.Connection | None = None,
    ) -> uuid.UUID | None:
        if _conn is None:
            async with self._database().acquire() as conn, conn.transaction():
                return await self.submit(
                    job_type,
                    payload,
                    priority=priority,
                    delay=delay,
                    fence=fence,
                    _conn=conn,
                )
        await self._group_lock(_conn, job_type, payload)
        if fence is not None:
            await require_claim(_conn, fence)
        return await _conn.fetchval(
            """INSERT INTO memory_job (job_type, payload, priority, available_at)
               VALUES ($1,$2,$3, clock_timestamp() + $4::interval)
               ON CONFLICT DO NOTHING RETURNING id""",
            job_type.value,
            payload,
            priority,
            delay or timedelta(),
        )

    async def claim(self, *, lease: timedelta = LEASE) -> Job | None:
        if lease <= timedelta():
            raise ValueError("job lease must be positive")
        db = self._database()
        await db.execute(
            """WITH expired AS (
                   SELECT id FROM memory_job WHERE status='running'
                     AND lease_until <= clock_timestamp() AND retry_count >= max_retry
                   ORDER BY lease_until FOR UPDATE SKIP LOCKED LIMIT 32
               )
               UPDATE memory_job j SET status='dead', finished_at=clock_timestamp(),
                   locked_by=NULL, locked_at=NULL, claim_token=NULL, lease_until=NULL,
                   last_error='lease expired after retry exhaustion'
               FROM expired WHERE j.id=expired.id"""
        )
        row = await db.fetchrow(
            """WITH picked AS (
                   SELECT id FROM memory_job
                    WHERE (status='pending' AND available_at <= clock_timestamp())
                       OR (status='running' AND lease_until <= clock_timestamp()
                           AND retry_count < max_retry)
                    ORDER BY priority DESC, available_at, id
                    FOR UPDATE SKIP LOCKED LIMIT 1
               )
               UPDATE memory_job j
                  SET status='running', locked_at=clock_timestamp(), locked_by=$1,
                      claim_token=$2, lease_until=clock_timestamp() + $3::interval,
                      retry_count=j.retry_count + CASE WHEN j.status='running' THEN 1 ELSE 0 END
                 FROM picked WHERE j.id=picked.id
               RETURNING j.id, j.job_type, j.payload, j.retry_count, j.max_retry, j.claim_token""",
            self._worker,
            uuid.uuid4(),
            lease,
        )
        return Job(**{**dict(row), "job_type": JobType(row["job_type"])}) if row else None

    async def renew(self, job: Job, *, lease: timedelta = LEASE) -> bool:
        if lease <= timedelta():
            raise ValueError("job lease must be positive")
        try:
            async with self._database().acquire() as conn, conn.transaction():
                await require_claim(conn, job)
                await conn.execute(
                    "UPDATE memory_job SET lease_until=clock_timestamp()+$2::interval WHERE id=$1",
                    job.id,
                    lease,
                )
                return True
        except LeaseLost:
            return False

    async def done(self, job: Job) -> bool:
        try:
            async with self._database().acquire() as conn, conn.transaction():
                await require_claim(conn, job)
                await conn.execute(
                    """UPDATE memory_job SET status='done', finished_at=clock_timestamp(),
                       locked_by=NULL, locked_at=NULL, claim_token=NULL, lease_until=NULL
                       WHERE id=$1""",
                    job.id,
                )
                return True
        except LeaseLost:
            return False

    async def fail(self, job: Job, error: str, *, backoff: timedelta) -> None:
        await self._reschedule(job, error, backoff=backoff, failed=True)

    async def defer(self, job: Job, reason: str, *, backoff: timedelta) -> None:
        """Delay the same logical work without consuming or resetting retry fuel."""
        await self._reschedule(job, reason, backoff=backoff, failed=False)

    async def _reschedule(self, job: Job, error: str, *, backoff: timedelta, failed: bool) -> None:
        async with self._database().acquire() as conn, conn.transaction():
            await self._group_lock(conn, job.job_type, job.payload)
            await require_claim(conn, job)
            twin = await conn.fetchrow(
                """SELECT id, retry_count, max_retry FROM memory_job
                    WHERE status='pending' AND job_type=$1
                      AND payload->>'group_id'=$2 AND id<>$3 FOR UPDATE""",
                job.job_type.value,
                str(job.payload["group_id"]) if "group_id" in job.payload else None,
                job.id,
            )
            attempts = job.retry_count + int(failed)
            exhausted = failed and job.exhausted
            if twin is not None:
                attempts = max(attempts, twin["retry_count"])
                maximum = min(job.max_retry, twin["max_retry"])
                exhausted = exhausted or attempts > maximum
                await conn.execute(
                    """UPDATE memory_job SET retry_count=$2, max_retry=$3,
                           available_at=GREATEST(available_at, clock_timestamp() + $4::interval),
                           status=$5::text, last_error=$6,
                           finished_at=CASE WHEN $5='dead' THEN clock_timestamp() ELSE NULL END
                        WHERE id=$1""",
                    twin["id"],
                    attempts,
                    maximum,
                    backoff,
                    "dead" if exhausted else "pending",
                    error[:2000],
                )
                error = "yielded to a newer pending twin: " + error
            terminal = exhausted or twin is not None
            await conn.execute(
                """UPDATE memory_job SET status=$3::text, retry_count=$4,
                       available_at=clock_timestamp() + $5::interval, last_error=$6,
                       locked_by=NULL, locked_at=NULL, claim_token=NULL, lease_until=NULL,
                       finished_at=CASE WHEN $3='dead' THEN clock_timestamp() ELSE NULL END
                    WHERE id=$1 AND claim_token=$2""",
                job.id,
                job.claim_token,
                "dead" if terminal else "pending",
                attempts,
                backoff,
                error[:2000],
            )

    async def purge_done(self, *, days: int) -> int:
        tag = await self._database().execute(
            "DELETE FROM memory_job WHERE status='done' AND finished_at < NOW() - $1::interval",
            timedelta(days=days),
        )
        return int(tag.split()[-1])

    async def depth(self, *, job_types: tuple[JobType, ...] | None = None) -> dict[str, int]:
        rows = await self._database().fetch(
            """SELECT status, count(*) AS n FROM memory_job
                WHERE $1::text[] IS NULL OR job_type=ANY($1::text[]) GROUP BY status""",
            [kind.value for kind in job_types] if job_types else None,
        )
        return {row["status"]: row["n"] for row in rows}
