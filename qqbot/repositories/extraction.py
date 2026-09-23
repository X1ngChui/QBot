"""Exact-event memory extraction batches and staged model output."""

from __future__ import annotations

import uuid
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from datetime import timedelta

import asyncpg

from ..db import pool
from ..domain.ids import GroupId
from ..domain.memory import (
    Candidate,
    CandidateStatus,
    CandidateType,
    ExtractionBatch,
    ExtractionSnapshot,
    ExtractionStatus,
)
from .archive import archive_columns, archived_messages


class ExtractionRepository:
    @asynccontextmanager
    async def model_slot(self, group_id: GroupId) -> AsyncIterator[bool]:
        """Hold the per-group provider slot without holding a transaction.

        Exact membership makes a batch durable, but it does not by itself stop two
        workers from opening that same extracting batch and paying for it together.
        A session advisory lock closes that gap. The connection is held across the
        provider call, while no transaction is: process death releases the lock and
        leaves the durable batch available to the next worker.
        """

        key = f"memory-provider:{group_id}"
        async with pool().acquire() as conn:
            acquired = bool(
                await conn.fetchval(
                    "SELECT pg_try_advisory_lock(hashtextextended($1, 0))",
                    key,
                )
            )
            try:
                yield acquired
            finally:
                if acquired:
                    await conn.execute(
                        "SELECT pg_advisory_unlock(hashtextextended($1, 0))",
                        key,
                    )

    async def open(self, group_id: GroupId) -> ExtractionBatch | None:
        row = await pool().fetchrow(
            """SELECT * FROM memory_extraction
                WHERE group_id=$1 AND status IN ('extracting','staged')
                ORDER BY started_at, id LIMIT 1""",
            group_id.to_db(),
        )
        return await self._batch(row) if row else None

    async def unconsumed_count(self, group_id: GroupId) -> int:
        return int(
            await pool().fetchval(
                """SELECT count(*) FROM raw_event r
                WHERE r.group_id=$1 AND r.event_type IN ('message','notice')
                  AND NOT EXISTS (
                      SELECT 1 FROM memory_extraction_event used
                       WHERE used.raw_event_id=r.id
                  )""",
                group_id.to_db(),
            )
            or 0
        )

    async def claim(
        self,
        group_id: GroupId,
        *,
        limit: int,
        floor: int,
        gap: timedelta,
    ) -> ExtractionBatch | None:
        """Reserve one exact batch before any provider call."""

        async with pool().acquire() as conn, conn.transaction():
            await conn.execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
                f"memory-extraction:{group_id}",
            )
            existing = await conn.fetchrow(
                """SELECT * FROM memory_extraction
                    WHERE group_id=$1 AND status IN ('extracting','staged')
                    ORDER BY started_at, id LIMIT 1 FOR UPDATE""",
                group_id.to_db(),
            )
            if existing:
                return await self._batch(existing, conn=conn)

            rows = await conn.fetch(
                f"""SELECT {archive_columns("r")} FROM raw_event r
                    WHERE r.group_id=$1 AND r.event_type IN ('message','notice')
                      AND NOT EXISTS (
                          SELECT 1 FROM memory_extraction_event used
                           WHERE used.raw_event_id=r.id
                      )
                    ORDER BY r.created_at, r.id
                    LIMIT $2
                    FOR UPDATE OF r SKIP LOCKED""",
                group_id.to_db(),
                limit,
            )
            if not rows or len(rows) < floor:
                return None
            events = archived_messages(rows)
            if len(events) == limit:
                for index in range(len(events) - 1, limit // 2, -1):
                    if events[index].occurred_at - events[index - 1].occurred_at >= gap:
                        events = events[:index]
                        break

            extraction_id = await conn.fetchval(
                """INSERT INTO memory_extraction (group_id, status)
                   VALUES ($1,'extracting') RETURNING id""",
                group_id.to_db(),
            )
            await conn.executemany(
                """INSERT INTO memory_extraction_event
                       (extraction_id, raw_event_id, ordinal)
                   VALUES ($1,$2,$3)""",
                [
                    (extraction_id, event.raw_event_id, ordinal)
                    for ordinal, event in enumerate(events, 1)
                ],
            )
            return ExtractionBatch(
                id=extraction_id,
                group_id=group_id,
                status=ExtractionStatus.EXTRACTING,
                events=tuple(events),
            )

    async def stage(
        self,
        extraction_id: uuid.UUID,
        snapshot: ExtractionSnapshot,
        candidates: list[Candidate],
    ) -> None:
        """Checkpoint validated model output without applying any projection."""

        async with pool().acquire() as conn, conn.transaction():
            row = await conn.fetchrow(
                "SELECT status FROM memory_extraction WHERE id=$1 FOR UPDATE",
                extraction_id,
            )
            if row is None:
                raise RuntimeError(f"unknown extraction {extraction_id}")
            status = ExtractionStatus(row["status"])
            if status is ExtractionStatus.APPLIED:
                return
            if status is ExtractionStatus.STAGED:
                return
            for candidate in candidates:
                if candidate.extraction_id not in (None, extraction_id):
                    raise ValueError("candidate belongs to another extraction")
            if candidates:
                await conn.executemany(
                    """INSERT INTO memory_candidate
                           (id, extraction_id, group_id, source_event_id,
                            candidate_type, payload, confidence, status)
                       VALUES ($1,$2,$3,$4,$5,$6,$7,'pending')""",
                    [
                        (
                            candidate.id,
                            extraction_id,
                            candidate.group_id.to_db() if candidate.group_id is not None else None,
                            candidate.source_event_id,
                            candidate.candidate_type.value,
                            candidate.payload,
                            candidate.confidence,
                        )
                        for candidate in candidates
                    ],
                )
            await conn.execute(
                """UPDATE memory_extraction
                      SET status='staged', snapshot=$2, candidate_count=$3,
                          staged_at=NOW()
                    WHERE id=$1""",
                extraction_id,
                snapshot.as_payload(),
                len(candidates),
            )

    async def candidates(
        self,
        extraction_id: uuid.UUID,
        *,
        conn: asyncpg.Connection | None = None,
    ) -> list[Candidate]:
        executor = conn or pool()
        rows = await executor.fetch(
            """SELECT * FROM memory_candidate
                WHERE extraction_id=$1 ORDER BY created_at, id""",
            extraction_id,
        )
        return [
            Candidate(
                id=row["id"],
                extraction_id=row["extraction_id"],
                group_id=GroupId(row["group_id"]),
                source_event_id=row["source_event_id"],
                candidate_type=CandidateType(row["candidate_type"]),
                payload=row["payload"],
                confidence=row["confidence"],
                status=CandidateStatus(row["status"]),
                reject_reason=row["reject_reason"],
                created_at=row["created_at"],
            )
            for row in rows
        ]

    async def _batch(
        self,
        row,
        *,
        conn: asyncpg.Connection | None = None,
    ) -> ExtractionBatch:
        executor = conn or pool()
        event_rows = await executor.fetch(
            f"""SELECT {archive_columns("r")} FROM memory_extraction_event item
                 JOIN raw_event r ON r.id=item.raw_event_id
                WHERE item.extraction_id=$1 ORDER BY item.ordinal""",
            row["id"],
        )
        snapshot = (
            ExtractionSnapshot.from_payload(row["snapshot"])
            if row["snapshot"] is not None
            else None
        )
        return ExtractionBatch(
            id=row["id"],
            group_id=GroupId(row["group_id"]),
            status=ExtractionStatus(row["status"]),
            events=tuple(archived_messages(event_rows)),
            snapshot=snapshot,
        )
