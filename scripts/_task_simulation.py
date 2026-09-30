"""Deterministic task storage for public model evaluations and command tests."""

from dataclasses import replace
from datetime import datetime
import uuid

from qqbot.clock import Clock
from qqbot.configuration import TasksCfg
from qqbot.domain.ids import GroupId
from qqbot.repositories.scheduled_task import (
    TASK_PAGE_SIZE,
    ScheduledTask,
    TaskLimit,
    TaskPage,
    TaskStatus,
)


class FictionalTaskStorage:
    def __init__(self, *, clock: Clock, namespace: str) -> None:
        self.clock = clock
        self.namespace = namespace
        self.tasks: dict[uuid.UUID, ScheduledTask] = {}
        self.creations = 0
        self.operations: list[str] = []

    async def create(
        self,
        group_id: GroupId,
        intent: str,
        due_at: datetime,
        limits: TasksCfg,
        *,
        parent: ScheduledTask | None = None,
    ) -> ScheduledTask:
        self.operations.append("create")
        if parent is not None and parent.group_id != group_id:
            raise ValueError("a followup must keep its group")
        depth = parent.chain_depth + 1 if parent is not None else 0
        if depth > limits.max_chain_depth:
            raise TaskLimit("本群任务续链深度已达上限")
        if (
            sum(
                task.group_id == group_id and task.status is TaskStatus.PENDING
                for task in self.tasks.values()
            )
            >= limits.max_pending_per_group
        ):
            raise TaskLimit("本群待执行任务数量已达上限")
        self.creations += 1
        identifier = uuid.uuid5(uuid.NAMESPACE_DNS, f"{self.namespace}-new-{self.creations}")
        task = ScheduledTask(
            identifier,
            group_id,
            intent,
            due_at,
            parent.chain_id if parent is not None else identifier,
            depth,
            created_at=self.clock.now(),
        )
        self.tasks[identifier] = task
        return task

    async def active(self, group_id: GroupId, *, page: int = 1) -> TaskPage:
        self.operations.append("active")
        rows = sorted(
            (
                task
                for task in self.tasks.values()
                if task.group_id == group_id
                and task.status in {TaskStatus.PENDING, TaskStatus.RUNNING}
            ),
            key=lambda task: (task.due_at, task.id),
        )
        offset = (page - 1) * TASK_PAGE_SIZE
        return TaskPage(
            tuple(rows[offset : offset + TASK_PAGE_SIZE]), page, len(rows) > offset + TASK_PAGE_SIZE
        )

    async def get(self, group_id: GroupId, task_id: uuid.UUID) -> ScheduledTask | None:
        self.operations.append("get")
        task = self.tasks.get(task_id)
        return task if task is not None and task.group_id == group_id else None

    async def update(
        self,
        group_id: GroupId,
        task_id: uuid.UUID,
        *,
        intent: str | None = None,
        due_at: datetime | None = None,
    ) -> ScheduledTask | None:
        self.operations.append("update")
        task = self.tasks.get(task_id)
        if task is None or task.group_id != group_id or task.status is not TaskStatus.PENDING:
            return None
        task = replace(
            task,
            intent=task.intent if intent is None else intent,
            due_at=task.due_at if due_at is None else due_at,
        )
        self.tasks[task_id] = task
        return task

    async def cancel(self, task_id: uuid.UUID, group_id: GroupId) -> ScheduledTask | None:
        self.operations.append("cancel")
        task = self.tasks.get(task_id)
        if task is None or task.group_id != group_id or task.status is not TaskStatus.PENDING:
            return None
        task = replace(
            task, status=TaskStatus.CANCELLED, finished_at=self.clock.now(), outcome="cancelled"
        )
        self.tasks[task_id] = task
        return task
