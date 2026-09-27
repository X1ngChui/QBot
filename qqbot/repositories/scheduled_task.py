"""Durable, single-attempt group wakeups with account-scoped management."""

from __future__ import annotations

import uuid
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime, timedelta

import asyncpg

from qqbot.domain.ids import GroupId
from qqbot.configuration import TasksCfg


MIN_DELAY_SECONDS = 300


class TaskLimit(ValueError):
    """A task exceeds a durable group or account limit."""


@dataclass(frozen=True, slots=True)
class ScheduledTask:
    id: uuid.UUID
    group_id: GroupId
    creator_id: str
    intent: str
    due_at: datetime
    chain_id: uuid.UUID
    chain_depth: int


class ScheduledTaskRepository:
    def __init__(self, *, database: Callable[[], asyncpg.Pool]) -> None:
        self._database = database

    async def create(
        self,
        group_id: GroupId,
        creator_id: str,
        intent: str,
        due_at: datetime,
        limits: TasksCfg,
        *,
        parent: ScheduledTask | None = None,
    ) -> ScheduledTask:
        if parent is not None and (parent.group_id != group_id or parent.creator_id != creator_id):
            raise ValueError("a follow-up must keep its original group and creator")
        depth = parent.chain_depth + 1 if parent is not None else 0
        if depth > limits.max_chain_depth:
            raise TaskLimit("连续定时次数已达上限")
        task_id = uuid.uuid4()
        chain_id = parent.chain_id if parent is not None else task_id
        async with self._database().acquire() as conn, conn.transaction():
            await conn.execute("SELECT pg_advisory_xact_lock($1::bigint)", group_id.to_db())
            counts = await conn.fetchrow(
                """SELECT count(*) AS group_count,
                          count(*) FILTER (WHERE creator_id=$2) AS account_count
                     FROM scheduled_task WHERE group_id=$1 AND status='pending'""",
                group_id.to_db(),
                creator_id,
            )
            if counts["group_count"] >= limits.max_pending_per_group:
                raise TaskLimit("本群待执行任务已达上限")
            if counts["account_count"] >= limits.max_pending_per_account:
                raise TaskLimit("你的待执行任务已达上限")
            await conn.execute(
                """INSERT INTO scheduled_task
                       (id, group_id, creator_id, intent, due_at, chain_id, chain_depth)
                     VALUES ($1,$2,$3,$4,$5,$6,$7)""",
                task_id,
                group_id.to_db(),
                creator_id,
                intent,
                due_at,
                chain_id,
                depth,
            )
        return ScheduledTask(task_id, group_id, creator_id, intent, due_at, chain_id, depth)

    async def pending(self, group_id: GroupId, creator_id: str, *, owner: bool) -> list:
        return await self._database().fetch(
            """SELECT id, creator_id, intent, due_at FROM scheduled_task
                 WHERE group_id=$1 AND status='pending' AND ($3 OR creator_id=$2)
                 ORDER BY due_at, id LIMIT 50""",
            group_id.to_db(),
            creator_id,
            owner,
        )

    async def cancel(
        self, task_id: uuid.UUID, group_id: GroupId, creator_id: str, *, owner: bool
    ) -> bool:
        row = await self._database().fetchval(
            """UPDATE scheduled_task SET status='cancelled', finished_at=now(),
                      outcome='cancelled'
                 WHERE id=$1 AND group_id=$2 AND status='pending'
                   AND ($4 OR creator_id=$3) RETURNING id""",
            task_id,
            group_id.to_db(),
            creator_id,
            owner,
        )
        return row is not None

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
                """SELECT id, group_id, creator_id, intent, due_at, chain_id, chain_depth
                     FROM scheduled_task WHERE status='pending' AND due_at <= now()
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
            await conn.execute(
                """UPDATE scheduled_task SET status='running', started_at=now()
                     WHERE id=$1""",
                row["id"],
            )
        return (
            ScheduledTask(
                row["id"],
                group,
                row["creator_id"],
                row["intent"],
                row["due_at"],
                row["chain_id"],
                row["chain_depth"],
            ),
            False,
        )

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
