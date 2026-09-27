"""A busy group's pending wakeups do not occupy another group's execution slot."""

from datetime import timedelta
import uuid

import pytest

from qqbot.domain.ids import GroupId
from qqbot.repositories.scheduled_task import ScheduledTaskRepository
from _fixtures import now_local

pytestmark = pytest.mark.database


async def test_busy_groups_are_filtered_before_a_task_is_claimed(database, bundle, monkeypatch):
    ids = [uuid.uuid4() for _ in range(3)]
    for identifier, group in zip(ids, (311, 311, 312), strict=True):
        await database.pool.execute(
            """INSERT INTO scheduled_task(id,group_id,creator_id,intent,due_at,chain_id,chain_depth)
               VALUES ($1,$2,'101','Fictional',now()-interval '1 second',$1,0)""",
            identifier,
            group,
        )
    now = now_local()
    kwargs = {"day_start": now - timedelta(days=1), "day_end": now + timedelta(days=1)}
    task, skipped = await ScheduledTaskRepository(database=(lambda: database.pool)).claim_due(
        bundle.default.tasks,
        **kwargs,
        exclude_groups=(GroupId("311"),),
    )
    assert task.group_id == GroupId("312") and not skipped
    assert (
        await database.pool.fetchval(
            "SELECT count(*) FROM scheduled_task WHERE group_id=311 AND status='pending'"
        )
        == 2
    )
    assert await ScheduledTaskRepository(database=(lambda: database.pool)).claim_due(
        bundle.default.tasks,
        **kwargs,
        exclude_groups=(GroupId("311"), GroupId("312")),
    ) == (None, False)
