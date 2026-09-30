"""Shared task validation is clock-owned and independent of the speaking account."""

from datetime import UTC, datetime, timedelta
import uuid

import pytest

from qqbot.clock import Clock
from qqbot.configuration import TasksCfg
from qqbot.domain.ids import GroupId
from qqbot.repositories.scheduled_task import ScheduledTask, TaskPage
from qqbot.services.scheduled_tasks import ScheduledTaskService, TaskInputError

NOW = datetime(2030, 1, 2, 0, 0, tzinfo=UTC)
GROUP = GroupId("311")


class Repository:
    def __init__(self):
        self.calls = []
        self.task = ScheduledTask(uuid.uuid4(), GROUP, "Fictional goal", NOW, uuid.uuid4(), 2)

    async def create(self, group, intent, due, limits, *, parent=None):
        self.calls.append(("create", group, intent, due, parent))
        return self.task

    async def update(self, group, task_id, **patch):
        self.calls.append(("update", group, task_id, patch))
        return self.task

    async def active(self, group, *, page):
        self.calls.append(("active", group, page))
        return TaskPage((self.task,), page, False)

    async def get(self, group, task_id):
        return self.task

    async def cancel(self, task_id, group):
        return self.task


@pytest.fixture
def service():
    return ScheduledTaskService(Repository(), clock=Clock("Asia/Shanghai", wall=lambda: NOW))


@pytest.mark.parametrize("delay", [0, 1, 299, -1, True, 30 * 86400 + 1])
async def test_invalid_delay_does_not_write(service, delay):
    with pytest.raises(TaskInputError):
        await service.create(GROUP, "Future check", TasksCfg(), delay_seconds=delay)
    assert not service.repository.calls


@pytest.mark.parametrize("time", ["2030-01-02T00:10:00", "2030-01-02", "invalid"])
async def test_naive_or_invalid_time_does_not_write(service, time):
    with pytest.raises(TaskInputError):
        await service.create(GROUP, "Future check", TasksCfg(), run_at=time)
    assert not service.repository.calls


async def test_exact_minimum_uses_injected_clock_and_defangs_content(service):
    await service.create(GROUP, "  Check ⟦fictional⟧  ", TasksCfg(), delay_seconds=300)
    _, group, intent, due, parent = service.repository.calls[0]
    assert group is GROUP and intent == "Check [fictional]" and parent is None
    assert due == NOW + timedelta(seconds=300)


async def test_relative_delay_is_elapsed_time_across_dst_fold():
    instant = datetime(2030, 11, 3, 5, 58, tzinfo=UTC)
    service = ScheduledTaskService(
        Repository(),
        clock=Clock("America/New_York", wall=lambda: instant),
    )
    due = service.due(TasksCfg(), delay_seconds=300)
    assert due == instant + timedelta(seconds=300)
    assert due.astimezone(service.clock.zone).fold == 1


async def test_intent_only_update_does_not_revalidate_overdue_time(service):
    await service.update(GROUP, service.repository.task.id, TasksCfg(), intent="New goal")
    assert service.repository.calls[0][3] == {"intent": "New goal", "due_at": None}


@pytest.mark.parametrize(
    "patch",
    [{}, {"intent": " "}, {"intent": "x" * 501}, {"run_at": NOW.isoformat(), "delay_seconds": 600}],
)
async def test_rejected_patch_is_not_mutated(service, patch):
    with pytest.raises(TaskInputError):
        await service.update(GROUP, service.repository.task.id, TasksCfg(), **patch)
    assert not service.repository.calls


@pytest.mark.parametrize("page", [0, -1, 10001, True])
async def test_invalid_page_is_not_queried(service, page):
    with pytest.raises(TaskInputError):
        await service.active(GROUP, page=page)
    assert not service.repository.calls
