"""The background job queue.

No Kafka (design doc 48): PostgreSQL's `FOR UPDATE SKIP LOCKED` is already enough for
several workers to contend for one table safely, and one fewer piece of middleware is one
fewer thing to operate.

What this layer guarantees is that work in progress survives the process being killed:
every lease, retry and backoff lives in the table, never in the process.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass
from datetime import timedelta
from enum import StrEnum

import asyncpg

from ..db import pool


class JobType(StrEnum):
    EXTRACT_MEMORY = "extract_memory"     # read a batch of messages into candidates
    CONSOLIDATE = "consolidate"           # validate candidates and write them down
    EMBED = "embed"                       # fill in missing vectors
    DECAY = "decay"                       # age memories out


@dataclass(frozen=True, slots=True)
class Job:
    id: uuid.UUID
    job_type: JobType
    payload: dict
    retry_count: int
    max_retry: int

    @property
    def exhausted(self) -> bool:
        return self.retry_count >= self.max_retry


class JobQueue:
    def __init__(self, worker_id: str) -> None:
        self._worker = worker_id

    async def submit(
        self, job_type: JobType, payload: dict, *, priority: int = 0,
        delay: timedelta | None = None,
    ) -> uuid.UUID | None:
        """Queue one job. Returns None when an identical job is already pending.

        Deduplicated by a partial unique index on (job_type, group) over pending rows:
        the same work queued twice is the same work, and the callers that can collide -
        the nightly drain and an owner's /relearn, or a /relearn against a job still in
        retry backoff - are exactly the ones that must not pay twice. The collapsed
        caller's extra payload travels via amend_pending below.
        """
        return await pool().fetchval(
            """INSERT INTO memory_job (job_type, payload, priority, available_at)
               VALUES ($1,$2,$3, NOW() + $4::interval)
               ON CONFLICT DO NOTHING
               RETURNING id""",
            job_type.value, payload, priority, delay or timedelta(0),
        )

    async def amend_pending(self, job_type: JobType, group_id: int,
                            patch: dict) -> bool:
        """Merge extra payload into an already-pending twin.

        For the caller whose submit was collapsed by the dedup index but whose
        payload carried more than the twin's - /relearn's force flag must reach
        whichever job actually runs, or the owner's explicit ask silently degrades
        into the ordinary drain the pending job was queued for.
        """
        tag = await pool().execute(
            """UPDATE memory_job SET payload = payload || $3::jsonb
                WHERE status='pending' AND job_type=$1
                  AND payload->>'group_id' = $2""",
            job_type.value, str(group_id), patch,
        )
        return tag.endswith(" 1")

    async def claim(self, *, lease: timedelta = timedelta(minutes=10)) -> Job | None:
        """Take one job.

        SKIP LOCKED lets concurrent workers each take their own without blocking each
        other, and a job whose lease has expired is picked up again - so a worker crashing
        does not lock a job away forever.
        """
        # A running job whose lease keeps expiring is one that kills or hangs its
        # worker - fail() never runs for it, so without this sweep it would be
        # reclaimed every lease interval forever: exactly the burn-money-indefinitely
        # machine the max_retry cap exists to stop.
        await pool().execute(
            """UPDATE memory_job
                  SET status='dead', finished_at=NOW(), locked_by=NULL,
                      last_error=COALESCE(last_error,'')
                                 || ' [lease kept expiring past max_retry]'
                WHERE status='running' AND locked_at < NOW() - $1::interval
                  AND retry_count >= max_retry""",
            lease,
        )
        row = await pool().fetchrow(
            """WITH picked AS (
                   SELECT id FROM memory_job
                    WHERE (status='pending' AND available_at <= NOW())
                       OR (status='running' AND locked_at < NOW() - $2::interval)
                    ORDER BY priority DESC, available_at
                    FOR UPDATE SKIP LOCKED
                    LIMIT 1
               )
               UPDATE memory_job j
                  SET status='running', locked_at=NOW(), locked_by=$1,
                      retry_count = j.retry_count
                                    + CASE WHEN j.status = 'running' THEN 1 ELSE 0 END
                 FROM picked
                WHERE j.id = picked.id
            RETURNING j.id, j.job_type, j.payload, j.retry_count, j.max_retry""",
            self._worker, lease,
        )
        if row is None:
            return None
        return Job(
            id=row["id"], job_type=JobType(row["job_type"]),
            payload=row["payload"],
            retry_count=row["retry_count"], max_retry=row["max_retry"],
        )

    async def purge_done(self, *, days: int = 30) -> int:
        """Delete finished jobs older than `days`. Dead rows stay - they are the
        error record - but 'done' is pure history, and a table nothing prunes is
        scanned by every claim poll forever."""
        tag = await pool().execute(
            "DELETE FROM memory_job WHERE status='done' AND finished_at < NOW() - $1::interval",
            timedelta(days=days),
        )
        return int(tag.split()[-1] or 0)

    async def done(self, job_id: uuid.UUID) -> None:
        """Finish a job this worker still holds. The locked_by guard makes a stale
        worker's late completion a no-op instead of overwriting a reclaimant's run."""
        await pool().execute(
            """UPDATE memory_job SET status='done', finished_at=NOW(), locked_by=NULL
                WHERE id=$1 AND locked_by=$2""",
            job_id, self._worker,
        )

    async def fail(self, job: Job, error: str, *, backoff: timedelta) -> None:
        """Record one failure. Once the retries are exhausted it stops and keeps the last
        error - retrying forever turns one broken payload into a machine that burns money
        indefinitely.
        """
        if job.exhausted:
            await pool().execute(
                """UPDATE memory_job SET status='dead', last_error=$2,
                                         finished_at=NOW(), locked_by=NULL
                    WHERE id=$1 AND locked_by=$3""",
                job.id, error[:2000], self._worker,
            )
            return
        try:
            # The locked_by guard, like done()'s: a worker that hung past its lease
            # reports its failure late, after a reclaim - or after the sweep above
            # buried the job dead. Without the guard that stale report resurrected
            # a dead job to pending, buying runs past the max_retry cap the sweep
            # exists to enforce.
            await pool().execute(
                """UPDATE memory_job
                      SET status='pending', retry_count=retry_count+1,
                          available_at=NOW() + $3::interval, last_error=$2, locked_by=NULL
                    WHERE id=$1 AND locked_by=$4""",
                job.id, error[:2000], backoff, self._worker,
            )
        except asyncpg.UniqueViolationError:
            # A fresh pending twin was legally submitted while this one ran (the
            # dedupe index only sees pending rows), and demoting this one back to
            # pending would collide with it. The work is the twin's now: this row
            # steps aside with its error kept, instead of the exception escaping
            # step() and leaving the job stuck in 'running' with no error recorded.
            await pool().execute(
                """UPDATE memory_job SET status='dead', last_error=$2,
                                         finished_at=NOW(), locked_by=NULL
                    WHERE id=$1 AND locked_by=$3""",
                job.id, ("yielded to a newer pending twin: " + error)[:2000],
                self._worker,
            )

    async def depth(self) -> dict[str, int]:
        """How many jobs are in each state. Reported daily, because a stuck queue has no
        other symptom."""
        rows = await pool().fetch(
            "SELECT status, count(*) AS n FROM memory_job GROUP BY status")
        return {r["status"]: r["n"] for r in rows}
