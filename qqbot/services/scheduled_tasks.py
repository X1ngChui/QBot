"""Shared scheduling validation for model tools and direct commands."""

from __future__ import annotations

import uuid
from datetime import UTC, datetime, timedelta

from qqbot.clock import Clock
from qqbot.configuration import TasksCfg
from qqbot.domain.ids import GroupId
from typing import Protocol
from qqbot.repositories.scheduled_task import (
    MIN_DELAY_SECONDS,
    ScheduledTask,
    TaskPage,
)
from qqbot.util import defang


class TaskInputError(ValueError):
    """A scheduling request violates its public input contract."""


class TaskStorage(Protocol):
    async def create(
        self,
        group_id: GroupId,
        intent: str,
        due_at: datetime,
        limits: TasksCfg,
        *,
        parent: ScheduledTask | None = None,
    ) -> ScheduledTask: ...

    async def update(
        self,
        group_id: GroupId,
        task_id: uuid.UUID,
        *,
        intent: str | None = None,
        due_at: datetime | None = None,
    ) -> ScheduledTask | None: ...

    async def active(self, group_id: GroupId, *, page: int = 1) -> TaskPage: ...

    async def get(self, group_id: GroupId, task_id: uuid.UUID) -> ScheduledTask | None: ...

    async def cancel(self, task_id: uuid.UUID, group_id: GroupId) -> ScheduledTask | None: ...


class ScheduledTaskService:
    def __init__(self, repository: TaskStorage, *, clock: Clock) -> None:
        self.repository = repository
        self.clock = clock

    @staticmethod
    def intent(value: str) -> str:
        value = defang(value.strip())
        if not value or len(value) > 500:
            raise TaskInputError("任务内容须为 1 到 500 字。")
        return value

    def due(
        self, limits: TasksCfg, *, run_at: str | None = None, delay_seconds: int | None = None
    ) -> datetime:
        if (run_at is None) == (delay_seconds is None):
            raise TaskInputError("run_at 与 delay_seconds 必须且只能提供一个。")
        now = self.clock.now().astimezone(UTC)
        if delay_seconds is not None:
            if isinstance(delay_seconds, bool) or not isinstance(delay_seconds, int):
                raise TaskInputError("delay_seconds 必须是整数。")
            if not MIN_DELAY_SECONDS <= delay_seconds <= limits.max_days_ahead * 86400:
                raise TaskInputError("预约时间超出最短 300 秒或最远时距。")
            due = now + timedelta(seconds=delay_seconds)
        else:
            try:
                due = datetime.fromisoformat(run_at) if isinstance(run_at, str) else None
            except ValueError:
                due = None
            if due is None or due.utcoffset() is None:
                raise TaskInputError("run_at 必须是带时区的 ISO 8601 时间。")
        if not MIN_DELAY_SECONDS <= (due - now).total_seconds() <= limits.max_days_ahead * 86400:
            raise TaskInputError("预约时间超出最短 300 秒或最远时距。")
        return due

    async def create(
        self,
        group_id: GroupId,
        intent: str,
        limits: TasksCfg,
        *,
        run_at: str | None = None,
        delay_seconds: int | None = None,
        parent: ScheduledTask | None = None,
    ) -> ScheduledTask:
        text = self.intent(intent)
        due = self.due(limits, run_at=run_at, delay_seconds=delay_seconds)
        return await self.repository.create(group_id, text, due, limits, parent=parent)

    async def update(
        self,
        group_id: GroupId,
        task_id: uuid.UUID,
        limits: TasksCfg,
        *,
        intent: str | None = None,
        run_at: str | None = None,
        delay_seconds: int | None = None,
    ) -> ScheduledTask | None:
        if intent is None and run_at is None and delay_seconds is None:
            raise TaskInputError("修改任务至少需要内容或新的预约时间。")
        text = self.intent(intent) if intent is not None else None
        due = (
            self.due(limits, run_at=run_at, delay_seconds=delay_seconds)
            if run_at is not None or delay_seconds is not None
            else None
        )
        return await self.repository.update(group_id, task_id, intent=text, due_at=due)

    async def active(self, group_id: GroupId, *, page: int = 1) -> TaskPage:
        if isinstance(page, bool) or not 1 <= page <= 10000:
            raise TaskInputError("页码须为 1 到 10000 的整数。")
        return await self.repository.active(group_id, page=page)

    async def get(self, group_id: GroupId, task_id: uuid.UUID) -> ScheduledTask | None:
        return await self.repository.get(group_id, task_id)

    async def cancel(self, group_id: GroupId, task_id: uuid.UUID) -> ScheduledTask | None:
        return await self.repository.cancel(task_id, group_id)
