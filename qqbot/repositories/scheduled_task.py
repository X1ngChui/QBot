"""Durable, group-owned single-attempt wakeups."""

from __future__ import annotations

import uuid
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from datetime import datetime, timedelta
from enum import StrEnum
from typing import Any

import asyncpg

from qqbot.domain.ids import GroupId
from qqbot.configuration import TasksCfg

MIN_DELAY_SECONDS = 300
TASK_PAGE_SIZE = 5


class TaskLimit(ValueError):
    """A task exceeds a durable group or chain limit."""


class TaskStatus(StrEnum):
    PENDING = "pending"
    RUNNING = "running"
    DONE = "done"
    FAILED = "failed"
    CANCELLED = "cancelled"


@dataclass(frozen=True, slots=True)
class ScheduledTask:
    id: uuid.UUID
    group_id: GroupId
    intent: str
    due_at: datetime
    chain_id: uuid.UUID
    chain_depth: int
    status: TaskStatus = TaskStatus.PENDING
    created_at: datetime | None = None
    started_at: datetime | None = None
    finished_at: datetime | None = None
    outcome: str | None = None


@dataclass(frozen=True, slots=True)
class TaskPage:
    items: tuple[ScheduledTask, ...]
    page: int
    has_more: bool


def _task(row: Mapping[str, Any]) -> ScheduledTask:
    return ScheduledTask(
        row["id"],
        GroupId(row["group_id"]),
        row["intent"],
        row["due_at"],
        row["chain_id"],
        row["chain_depth"],
        TaskStatus(row["status"]),
        row["created_at"],
        row["started_at"],
        row["finished_at"],
        row["outcome"],
    )


class ScheduledTaskRepository:
    def __init__(self, *, database: Callable[[], asyncpg.Pool]) -> None:
        self._database = database

    async def create(
        self,
        group_id: GroupId,
        intent: str,
        due_at: datetime,
        limits: TasksCfg,
        *,
        parent: ScheduledTask | None = None,
    ) -> ScheduledTask:
        if parent is not None and parent.group_id != group_id:
            raise ValueError("a follow-up must keep its original group")
        depth = parent.chain_depth + 1 if parent is not None else 0
        if depth > limits.max_chain_depth:
            raise TaskLimit("本群任务续链深度已达上限")
        task_id = uuid.uuid4()
        chain_id = parent.chain_id if parent is not None else task_id
        async with self._database().acquire() as conn, conn.transaction():
            await conn.execute("SELECT pg_advisory_xact_lock($1::bigint)", group_id.to_db())
            count = await conn.fetchval(
                """SELECT count(*) FROM scheduled_task
                     WHERE group_id=$1 AND status='pending'""",
                group_id.to_db(),
            )
            if count >= limits.max_pending_per_group:
                raise TaskLimit("本群待执行任务数量已达上限")
            row = await conn.fetchrow(
                """INSERT INTO scheduled_task
                       (id, group_id, intent, due_at, chain_id, chain_depth)
                     VALUES ($1,$2,$3,$4,$5,$6) RETURNING *""",
                task_id,
                group_id.to_db(),
                intent,
                due_at,
                chain_id,
                depth,
            )
        if row is None:
            raise RuntimeError("task insert returned no row")
        return _task(row)

    async def active(self, group_id: GroupId, *, page: int = 1) -> TaskPage:
        if not 1 <= page <= 10000:
            raise ValueError("page must be between 1 and 10000")
        rows = await self._database().fetch(
            """SELECT * FROM scheduled_task
                 WHERE group_id=$1 AND status IN ('pending','running')
                 ORDER BY due_at, id LIMIT $2 OFFSET $3""",
            group_id.to_db(),
            TASK_PAGE_SIZE + 1,
            (page - 1) * TASK_PAGE_SIZE,
        )
        return TaskPage(
            tuple(_task(row) for row in rows[:TASK_PAGE_SIZE]), page, len(rows) > TASK_PAGE_SIZE
        )

    async def get(self, group_id: GroupId, task_id: uuid.UUID) -> ScheduledTask | None:
        row = await self._database().fetchrow(
            "SELECT * FROM scheduled_task WHERE group_id=$1 AND id=$2",
            group_id.to_db(),
            task_id,
        )
        return None if row is None else _task(row)

    async def update(
        self,
        group_id: GroupId,
        task_id: uuid.UUID,
        *,
        intent: str | None = None,
        due_at: datetime | None = None,
    ) -> ScheduledTask | None:
        if intent is None and due_at is None:
            raise ValueError("an update needs at least one field")
        row = await self._database().fetchrow(
            """UPDATE scheduled_task
                  SET intent=COALESCE($3,intent), due_at=COALESCE($4,due_at)
                 WHERE group_id=$1 AND id=$2 AND status='pending' RETURNING *""",
            group_id.to_db(),
            task_id,
            intent,
            due_at,
        )
        return None if row is None else _task(row)

    async def cancel(self, task_id: uuid.UUID, group_id: GroupId) -> ScheduledTask | None:
        row = await self._database().fetchrow(
            """UPDATE scheduled_task SET status='cancelled', finished_at=now(),
                      outcome='cancelled'
                 WHERE id=$1 AND group_id=$2 AND status='pending' RETURNING *""",
            task_id,
            group_id.to_db(),
        )
        return None if row is None else _task(row)

    async def claim_due(
        self,
        limits: TasksCfg,
        *,
        day_start: datetime,
        day_end: datetime,
        exclude_groups: tuple[GroupId, ...] = (),
    ) -> tuple[ScheduledTask | None, bool]:
        async with self._database().acquire() as conn, conn.transaction():
            row = await conn.fetchrow(
                """SELECT * FROM scheduled_task
                     WHERE status='pending' AND due_at <= now()
                       AND NOT (group_id = ANY($1::bigint[]))
                     ORDER BY due_at, id FOR UPDATE SKIP LOCKED LIMIT 1""",
                [group.to_db() for group in exclude_groups],
            )
            if row is None:
                return None, False
            group = GroupId(row["group_id"])
            await conn.execute("SELECT pg_advisory_xact_lock($1::bigint)", group.to_db())
            count = await conn.fetchval(
                """SELECT count(*) FROM scheduled_task
                     WHERE group_id=$1 AND started_at >= $2 AND started_at < $3""",
                group.to_db(),
                day_start,
                day_end,
            )
            if count >= limits.max_executions_per_group_day:
                await conn.execute(
                    """UPDATE scheduled_task SET status='done', finished_at=now(),
                              outcome='daily_limit' WHERE id=$1""",
                    row["id"],
                )
                return None, True
            row = await conn.fetchrow(
                """UPDATE scheduled_task SET status='running', started_at=now()
                     WHERE id=$1 RETURNING *""",
                row["id"],
            )
        if row is None:
            raise RuntimeError("claimed task disappeared")
        return _task(row), False

    async def finish(self, task_id: uuid.UUID, outcome: str, *, failed: bool = False) -> None:
        await self._database().execute(
            """UPDATE scheduled_task SET status=$2, outcome=$3, finished_at=now()
                 WHERE id=$1 AND status='running'""",
            task_id,
            "failed" if failed else "done",
            outcome,
        )

    async def interrupt(self) -> None:
        await self._database().execute(
            """UPDATE scheduled_task SET status='failed', outcome='interrupted',
                      finished_at=now() WHERE status='running'"""
        )

    async def purge(self, days: int) -> None:
        await self._database().execute(
            """DELETE FROM scheduled_task WHERE status IN ('done','failed','cancelled')
                 AND finished_at < now() - $1::interval""",
            timedelta(days=days),
        )
