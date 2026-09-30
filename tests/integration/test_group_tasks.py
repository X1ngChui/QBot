"""Group-owned task CRUD preserves lineage and conditional claim boundaries."""

import asyncio
from datetime import UTC, datetime, timedelta
import uuid

import pytest

from qqbot.clock import Clock
from qqbot.configuration import TasksCfg
from qqbot.domain.ids import GroupId
from qqbot.repositories.scheduled_task import ScheduledTaskRepository, TaskLimit, TaskStatus
from qqbot.services.scheduled_tasks import ScheduledTaskService

pytestmark = pytest.mark.database
GROUP = GroupId("311")
OTHER = GroupId("312")


def services(database):
    repository = ScheduledTaskRepository(database=lambda: database.pool)
    return repository, ScheduledTaskService(repository, clock=Clock("Asia/Shanghai"))


async def test_group_crud_and_pending_only_update_preserve_lineage(database):
    repo, service = services(database)
    limits = TasksCfg(max_pending_per_group=1)
    root = await service.create(GROUP, "Initial fictional goal", limits, delay_seconds=600)
    assert await service.get(OTHER, root.id) is None
    assert await service.cancel(OTHER, root.id) is None
    assert await service.update(OTHER, root.id, limits, intent="Other group") is None
    with pytest.raises(TaskLimit):
        await service.create(GROUP, "Additional fictional goal", limits, delay_seconds=600)
    edited = await service.update(GROUP, root.id, limits, intent="Revised goal", delay_seconds=900)
    assert edited.id == root.id and edited.chain_id == root.chain_id
    assert edited.chain_depth == root.chain_depth and edited.created_at == root.created_at
    assert edited.due_at > root.due_at and edited.intent == "Revised goal"
    cancelled = await service.cancel(GROUP, root.id)
    assert cancelled.status is TaskStatus.CANCELLED
    assert await service.cancel(GROUP, root.id) is None
    assert await service.update(GROUP, root.id, limits, intent="Cannot resurrect") is None
    assert (await service.get(GROUP, root.id)).status is TaskStatus.CANCELLED
    assert not (await repo.active(GROUP)).items


async def test_active_paging_includes_running_but_not_terminal(database):
    repo, service = services(database)
    tasks = [
        await service.create(GROUP, f"Fictional goal {index}", TasksCfg(), delay_seconds=600)
        for index in range(8)
    ]
    await database.pool.execute(
        "UPDATE scheduled_task SET status='running' WHERE id=$1",
        tasks[0].id,
    )
    await service.cancel(GROUP, tasks[-1].id)
    first = await repo.active(GROUP)
    second = await repo.active(GROUP, page=2)
    assert len(first.items) == 5 and first.has_more
    assert len(second.items) == 2 and not second.has_more
    assert any(task.status is TaskStatus.RUNNING for task in first.items + second.items)
    assert {task.id for task in first.items}.isdisjoint(task.id for task in second.items)
    assert await repo.update(GROUP, tasks[0].id, intent="No running edit") is None
    assert await repo.cancel(tasks[0].id, GROUP) is None
    assert not (await repo.active(OTHER)).items


async def test_concurrent_create_respects_group_cap_and_chain_scope(database):
    repo, service = services(database)
    limits = TasksCfg(max_pending_per_group=1, max_chain_depth=1)
    offered = await asyncio.gather(
        *(
            service.create(GROUP, f"Fictional goal {index}", limits, delay_seconds=600)
            for index in range(5)
        ),
        return_exceptions=True,
    )
    assert sum(isinstance(value, TaskLimit) for value in offered) == 4
    root = next(value for value in offered if not isinstance(value, BaseException))
    with pytest.raises(ValueError):
        await repo.create(OTHER, "Wrong group followup", root.due_at, limits, parent=root)
    await repo.cancel(root.id, GROUP)
    child = await service.create(GROUP, "Followup", limits, delay_seconds=600, parent=root)
    assert child.chain_id == root.chain_id and child.chain_depth == 1
    await repo.cancel(child.id, GROUP)
    with pytest.raises(TaskLimit):
        await service.create(GROUP, "Too deep", limits, delay_seconds=600, parent=child)


@pytest.mark.parametrize("operation", ["update", "cancel"])
async def test_mutation_races_claim_without_deadlock_or_resurrection(database, operation):
    repo, _service = services(database)
    now = datetime.now(UTC)
    task = await repo.create(GROUP, "Due fictional goal", now - timedelta(seconds=1), TasksCfg())
    day_start = now.replace(hour=0, minute=0, second=0, microsecond=0)
    mutation = (
        repo.update(GROUP, task.id, due_at=now + timedelta(hours=1))
        if operation == "update"
        else repo.cancel(task.id, GROUP)
    )
    changed, (claimed, exhausted) = await asyncio.wait_for(
        asyncio.gather(
            mutation,
            repo.claim_due(TasksCfg(), day_start=day_start, day_end=day_start + timedelta(days=1)),
        ),
        timeout=3,
    )
    assert not exhausted
    assert (changed is None) != (claimed is None)
    row = await repo.get(GROUP, task.id)
    if claimed is not None:
        assert row.status is TaskStatus.RUNNING
        assert await repo.update(GROUP, task.id, intent="Too late") is None
        assert await repo.cancel(task.id, GROUP) is None
    else:
        assert row.status is (TaskStatus.PENDING if operation == "update" else TaskStatus.CANCELLED)
    assert row.chain_id == task.chain_id and row.chain_depth == task.chain_depth


async def test_missing_uuid_is_not_a_success(database):
    repo, _service = services(database)
    absent = uuid.uuid4()
    assert await repo.get(GROUP, absent) is None
    assert await repo.update(GROUP, absent, intent="Fictional") is None
    assert await repo.cancel(absent, GROUP) is None
