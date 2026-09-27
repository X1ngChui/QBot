"""Durable wakeups share reply capacity, deadlines and side-effect accounting."""

import asyncio
from types import SimpleNamespace
from unittest.mock import AsyncMock
import uuid

import pytest

from _budget import fake_budget
import _db as _test_db
from qqbot.conversation import engine
from qqbot.conversation.history import HistoryWindow
from qqbot.conversation.scheduler import Admission, ReplyScheduler
from qqbot.conversation.session import ReplyExecutor
from qqbot.domain.ids import GroupId
from qqbot.domain.reply import ReplyEnd
from qqbot.repositories.scheduled_task import ScheduledTask
from qqbot.services.members import MemberDirectory
from qqbot.workers.scheduled import ScheduledTaskWorker


def task(group):
    return ScheduledTask(
        uuid.uuid4(), GroupId(group), "101", "Fictional", _test_db.clock.now(), uuid.uuid4(), 0
    )


def runner(bundle, cfg=None, *, capacity=4):
    state = SimpleNamespace(
        muted=False,
        load_history=AsyncMock(),
        blocked_now=AsyncMock(return_value=False),
        recent=[],
        history=HistoryWindow(),
        history_anchor=None,
    )
    executor = ReplyExecutor(
        bundle=_test_db.bundle_for_settings(cfg or bundle.default),
        clock=_test_db.clock,
        database=_test_db.pool,
        identities=_test_db.identities,
        evidence_store=_test_db.evidence,
        archive=_test_db.archive,
        budget=fake_budget(),
        members=MemberDirectory(),
        registry=SimpleNamespace(get=AsyncMock(return_value=state)),
        delivery=object(),
        media=SimpleNamespace(settle=AsyncMock(), processor=object()),
        providers=object(),
        directory=object(),
    )
    replies = ReplyScheduler(executor, capacity=capacity, concurrency=2)
    service = ScheduledTaskWorker(
        cfg or bundle.default,
        replies,
        database=_test_db.pool,
        clock=_test_db.clock,
    )
    service._repo = SimpleNamespace(finish=AsyncMock(), interrupt=AsyncMock(), purge=AsyncMock())
    return service, state, executor


def submit(service, due, *, timeout=None):
    reservation = service._replies.reserve()
    assert reservation is not None
    return service.submit(
        SimpleNamespace(self_id="999"),
        due,
        reservation=reservation,
        deadline=asyncio.get_running_loop().time()
        + (service._cfg.conversation.reply_deadline_sec if timeout is None else timeout),
    )


async def close(service):
    await service.stop()
    await service._replies.close()
    await service.close()


async def test_two_tasks_in_one_group_do_not_occupy_two_execution_slots(bundle):
    service, _, _ = runner(bundle)
    pending = [task("311"), task("311"), task("312")]
    claimed, started = [], []
    ready = asyncio.Event()

    async def claim(*args, exclude_groups=(), **kwargs):
        for candidate in pending:
            if candidate.id not in claimed and candidate.group_id not in exclude_groups:
                claimed.append(candidate.id)
                return candidate, False
        return None, False

    async def execute(work):
        started.append(work.value.group_id)
        if len(started) == 2:
            ready.set()
        await asyncio.Event().wait()

    service._repo.claim_due = claim
    service._replies._execute = execute
    try:
        await service.start(lambda: object())
        await asyncio.wait_for(ready.wait(), 1)
        assert started == [GroupId("311"), GroupId("312")]
        assert pending[1].id not in claimed
    finally:
        await close(service)
    with pytest.raises(RuntimeError, match="closed"):
        await service.start(lambda: object())
    assert service._repo.finish.await_count == 2
    assert not service._active_groups


async def test_deadline_includes_history_loading(bundle):
    service, state, _ = runner(bundle)

    async def load(**kwargs):
        await asyncio.Event().wait()

    state.load_history.side_effect = load
    due = task("311")
    try:
        submit(service, due, timeout=0.02)
        await service._replies.drain()
        await service.flush()
        service._repo.finish.assert_awaited_once_with(due.id, "timeout", failed=True)
    finally:
        await close(service)


async def test_acknowledged_send_survives_a_later_timer_failure(bundle, monkeypatch):
    service, _, executor = runner(bundle)
    monkeypatch.setattr(executor.budget, "exceeded", AsyncMock(return_value=False))

    async def respond(**kwargs):
        kwargs["progress"].confirm()
        assert kwargs["deadline"] > asyncio.get_running_loop().time()
        raise TimeoutError("synthetic timeout after ACK")

    monkeypatch.setattr(engine, "respond", respond)
    due = task("311")
    try:
        submit(service, due)
        await service._replies.drain()
        await service.flush()
        service._repo.finish.assert_awaited_once_with(due.id, "sent_timeout", failed=True)
    finally:
        await close(service)


async def test_saturated_shared_inbox_does_not_claim_a_durable_task(bundle):
    service, _, _ = runner(bundle, capacity=1)
    entered, polled = asyncio.Event(), asyncio.Event()

    async def execute(work):
        entered.set()
        await asyncio.Event().wait()

    async def pause(seconds):
        polled.set()
        await asyncio.Event().wait()

    service._replies._execute = execute
    service._pause = pause
    service._repo.claim_due = AsyncMock(side_effect=AssertionError("must remain pending in SQL"))
    try:
        service._replies.submit(object(), deadline=asyncio.get_running_loop().time() + 5)
        await entered.wait()
        await service.start(lambda: object())
        await asyncio.wait_for(polled.wait(), 1)
        service._repo.claim_due.assert_not_awaited()
        assert service._replies.size == service._replies.high_water == 1
    finally:
        await close(service)


async def test_claim_in_progress_holds_capacity_against_new_addressed_messages(bundle):
    service, _, _ = runner(bundle, capacity=1)
    entered = asyncio.Event()

    async def claim(*args, **kwargs):
        entered.set()
        await asyncio.Event().wait()

    service._repo.claim_due = claim
    try:
        await service.start(lambda: object())
        await asyncio.wait_for(entered.wait(), 1)
        assert service._replies.size == 1 and service._replies.active == 0
        assert (
            service._replies.submit(object(), deadline=asyncio.get_running_loop().time() + 5)
            is Admission.OVERLOADED
        )
    finally:
        await close(service)
    assert service._replies.size == 0


async def test_queued_timer_cancel_is_persisted_without_starting_execution(bundle):
    service, _, _ = runner(bundle, capacity=2)
    entered = asyncio.Event()
    service._replies._concurrency = 1

    async def execute(work):
        entered.set()
        await asyncio.Event().wait()

    service._replies._execute = execute
    due = task("311")
    service._replies.submit(object(), deadline=asyncio.get_running_loop().time() + 5)
    await entered.wait()
    submit(service, due)
    await close(service)
    service._repo.finish.assert_awaited_once_with(due.id, "interrupted", failed=True)


async def test_failed_terminal_write_keeps_bounded_receipt_for_idempotent_retry(bundle):
    service, _, _ = runner(bundle)
    due = task("311")

    async def execute(work):
        return work.progress.finish(ReplyEnd.FINISHED)

    service._replies._execute = execute
    try:
        submit(service, due)
        await service._replies.drain()
        service._repo.finish.side_effect = RuntimeError("temporary database failure")
        with pytest.raises(RuntimeError, match="temporary"):
            await service.flush()
        assert len(service._completed) == 1 and due.group_id in service._active_groups
        service._repo.finish.side_effect = None
        await service.flush()
        assert not service._completed and not service._active_groups
    finally:
        await close(service)
