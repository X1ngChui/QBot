"""Deterministic admission, expiry, cancellation and capacity contracts."""

import asyncio

from hypothesis import given, settings, strategies as st

from qqbot.conversation.scheduler import Admission, ReplyScheduler
from qqbot.domain.reply import ReplyEnd


async def test_capacity_covers_active_and_pending_without_waiter_tasks():
    entered, release = asyncio.Event(), asyncio.Event()
    finished = []

    async def execute(work):
        entered.set()
        await release.wait()
        return work.progress.finish(ReplyEnd.FINISHED)

    scheduler = ReplyScheduler(
        execute,
        capacity=3,
        concurrency=1,
        on_finish=lambda value, result: finished.append((value, result)),
    )
    try:
        deadline = asyncio.get_running_loop().time() + 5
        assert scheduler.submit(1, deadline=deadline) is Admission.ACCEPTED
        await entered.wait()
        assert scheduler.submit(2, deadline=deadline) is Admission.ACCEPTED
        assert scheduler.submit(3, deadline=deadline) is Admission.ACCEPTED
        assert scheduler.submit(4, deadline=deadline) is Admission.OVERLOADED
        assert scheduler.active == 1 and scheduler.size == 3
        release.set()
        await scheduler.drain()
        assert [value for value, result in finished if result.end is ReplyEnd.FINISHED] == [1, 2, 3]
        assert scheduler.high_water == 3
    finally:
        await scheduler.close()
    assert scheduler.submit(5, deadline=deadline) is Admission.CLOSED
    assert scheduler.size == 0


async def test_pending_deadline_expires_while_all_execution_slots_are_busy():
    entered, release, expired = asyncio.Event(), asyncio.Event(), asyncio.Event()
    executed = []

    async def execute(work):
        executed.append(work.value)
        entered.set()
        await release.wait()
        return work.progress.finish(ReplyEnd.FINISHED)

    def completed(value, outcome):
        if value == "waiting":
            assert outcome.end is ReplyEnd.EXPIRED
            expired.set()

    scheduler = ReplyScheduler(execute, capacity=2, concurrency=1, on_finish=completed)
    try:
        now = asyncio.get_running_loop().time()
        scheduler.submit("active", deadline=now + 5)
        await entered.wait()
        scheduler.submit("waiting", deadline=now + 0.02)
        await asyncio.wait_for(expired.wait(), 1)
        assert executed == ["active"]
        assert scheduler.size == 1
    finally:
        release.set()
        await scheduler.close()


async def test_timeout_preserves_ack_and_failure_isolates_siblings():
    entered, never = asyncio.Event(), asyncio.Event()
    outcomes = {}

    async def execute(work):
        if work.value == "sent":
            work.progress.confirm()
            entered.set()
            await never.wait()
        if work.value == "broken":
            raise RuntimeError("fictional independent session failure")
        return work.progress.finish(ReplyEnd.FINISHED)

    scheduler = ReplyScheduler(
        execute,
        capacity=3,
        concurrency=2,
        on_finish=lambda value, result: outcomes.update({value: result}),
    )
    try:
        now = asyncio.get_running_loop().time()
        scheduler.submit("sent", deadline=now + 0.02)
        await entered.wait()
        scheduler.submit("broken", deadline=now + 5)
        scheduler.submit("healthy", deadline=now + 5)
        await scheduler.drain()
        assert outcomes["sent"].end is ReplyEnd.TIMEOUT
        assert outcomes["sent"].acknowledged == 1
        assert outcomes["broken"].end is ReplyEnd.FAILED
        assert outcomes["healthy"].end is ReplyEnd.FINISHED
    finally:
        await scheduler.close()


async def test_shutdown_joins_active_sessions_and_discards_pending_work():
    entered, never = asyncio.Event(), asyncio.Event()
    finalized = []
    outcomes = []

    async def execute(work):
        entered.set()
        try:
            await never.wait()
        finally:
            finalized.append(work.value)
        return work.progress.finish(ReplyEnd.FINISHED)

    scheduler = ReplyScheduler(
        execute,
        capacity=2,
        concurrency=1,
        on_finish=lambda value, result: outcomes.append(result.end),
    )
    now = asyncio.get_running_loop().time()
    scheduler.submit("running", deadline=now + 5)
    await entered.wait()
    scheduler.submit("waiting", deadline=now + 5)
    await scheduler.close()
    await scheduler.close()
    assert finalized == ["running"]
    assert outcomes == [ReplyEnd.CANCELLED, ReplyEnd.CANCELLED]
    assert scheduler.size == scheduler.active == 0


@settings(max_examples=40, deadline=None)
@given(capacity=st.integers(1, 32), count=st.integers(0, 80))
def test_admission_has_a_hard_total_bound(capacity, count):
    async def scenario():
        async def execute(work):
            return work.progress.finish(ReplyEnd.FINISHED)

        scheduler = ReplyScheduler(execute, capacity=capacity, concurrency=capacity)
        try:
            deadline = asyncio.get_running_loop().time() + 5
            for index in range(count):
                result = scheduler.submit(index, deadline=deadline)
                assert (result is Admission.ACCEPTED) == (index < capacity)
                assert scheduler.size <= capacity
            assert scheduler.high_water == min(capacity, count)
        finally:
            await scheduler.close()

    asyncio.run(scenario())
