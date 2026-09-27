"""Reservation ownership across admission, draining, and shutdown."""

import asyncio

import pytest

from qqbot.conversation.scheduler import Admission, ReplyScheduler
from qqbot.domain.reply import ReplyEnd


def deadline(seconds=5):
    return asyncio.get_running_loop().time() + seconds


@pytest.mark.asyncio
async def test_reserve_cancel_submit_and_reuse_preserve_capacity_and_outcomes():
    observed = []

    async def execute(work):
        return work.progress.finish(ReplyEnd.FINISHED)

    scheduler = ReplyScheduler(
        execute,
        capacity=2,
        concurrency=1,
        on_finish=lambda value, outcome: observed.append((value, outcome.end)),
    )
    try:
        discarded = scheduler.reserve()
        claimed = scheduler.reserve()
        assert discarded is not None and claimed is not None
        assert scheduler.size == scheduler.high_water == 2
        assert scheduler.active == 0
        assert scheduler.submit("addressed", deadline=deadline()) is Admission.OVERLOADED

        discarded.cancel()
        discarded.cancel()
        assert scheduler.size == 1
        with pytest.raises(RuntimeError, match="already been released"):
            discarded.submit("stale", deadline=deadline())

        renewed = scheduler.reserve()
        assert renewed is not None and scheduler.size == 2
        assert claimed.submit("timer", deadline=deadline()) is Admission.ACCEPTED
        assert scheduler.size == 2
        with pytest.raises(RuntimeError, match="already been released"):
            claimed.submit("duplicate", deadline=deadline())
        claimed.cancel()
        assert scheduler.size == 2
        renewed.cancel()
        assert scheduler.size == 1
        await asyncio.wait_for(scheduler.drain(), 1)
        assert scheduler.size == scheduler.active == 0
        assert observed == [
            ("addressed", ReplyEnd.OVERLOADED),
            ("timer", ReplyEnd.FINISHED),
        ]
        assert scheduler.outcomes[ReplyEnd.OVERLOADED] == 1
        assert scheduler.outcomes[ReplyEnd.FINISHED] == 1
    finally:
        await scheduler.close()


@pytest.mark.asyncio
async def test_expired_reservation_reports_once_and_releases_slot():
    observed = []

    async def execute(work):
        pytest.fail(f"expired reservation unexpectedly executed {work.value!r}")

    scheduler = ReplyScheduler(execute, capacity=1, concurrency=1)
    try:
        reserved = scheduler.reserve()
        assert reserved is not None
        assert (
            reserved.submit(
                "timer",
                deadline=deadline(-1),
                on_finish=lambda value, outcome: observed.append((value, outcome.end)),
            )
            is Admission.EXPIRED
        )
        assert observed == [("timer", ReplyEnd.EXPIRED)]
        assert scheduler.size == 0 and scheduler.active == 0
        with pytest.raises(RuntimeError, match="already been released"):
            reserved.submit("again", deadline=deadline())
        assert scheduler.reserve() is not None
    finally:
        await scheduler.close()


@pytest.mark.asyncio
async def test_transfer_to_running_work_does_not_wake_drain():
    entered, release = asyncio.Event(), asyncio.Event()

    async def execute(work):
        entered.set()
        await release.wait()
        return work.progress.finish(ReplyEnd.FINISHED)

    scheduler = ReplyScheduler(execute, capacity=1, concurrency=1)
    waiter = None
    try:
        reserved = scheduler.reserve()
        assert reserved is not None
        waiter = asyncio.create_task(scheduler.drain())
        await asyncio.sleep(0)
        assert not waiter.done()
        assert reserved.submit("timer", deadline=deadline()) is Admission.ACCEPTED
        await asyncio.wait_for(entered.wait(), 1)
        assert scheduler.size == scheduler.active == 1
        assert not waiter.done()
        assert scheduler.submit("addressed", deadline=deadline()) is Admission.OVERLOADED
        assert not waiter.done()
        release.set()
        await asyncio.wait_for(waiter, 1)
        assert scheduler.size == scheduler.active == 0
    finally:
        release.set()
        await scheduler.close()
        if waiter is not None:
            await asyncio.gather(waiter, return_exceptions=True)


@pytest.mark.asyncio
async def test_close_releases_waiting_reservations_and_rejects_late_transfer():
    observed = []

    async def execute(work):
        pytest.fail(f"closed scheduler unexpectedly executed {work.value!r}")

    scheduler = ReplyScheduler(execute, capacity=1, concurrency=1)
    waiter = None
    try:
        reserved = scheduler.reserve()
        assert reserved is not None
        waiter = asyncio.create_task(scheduler.drain())
        await asyncio.sleep(0)
        assert not waiter.done()
        await scheduler.close()
        await asyncio.wait_for(waiter, 1)
        assert scheduler.size == scheduler.active == 0
        assert scheduler.reserve() is None
        assert (
            reserved.submit(
                "late timer",
                deadline=deadline(),
                on_finish=lambda value, outcome: observed.append((value, outcome.end)),
            )
            is Admission.CLOSED
        )
        assert observed == [("late timer", ReplyEnd.CANCELLED)]
        reserved.cancel()
        assert scheduler.size == 0
    finally:
        await scheduler.close()
        if waiter is not None:
            await asyncio.gather(waiter, return_exceptions=True)


@pytest.mark.asyncio
async def test_abort_with_active_work_and_reservation_cancels_once():
    entered, never = asyncio.Event(), asyncio.Event()
    observed = []

    async def execute(work):
        entered.set()
        await never.wait()
        return work.progress.finish(ReplyEnd.FINISHED)

    scheduler = ReplyScheduler(
        execute,
        capacity=2,
        concurrency=1,
        on_finish=lambda value, outcome: observed.append((value, outcome.end)),
    )
    try:
        assert scheduler.submit("active", deadline=deadline()) is Admission.ACCEPTED
        await asyncio.wait_for(entered.wait(), 1)
        token = scheduler.reserve()
        assert token is not None
        assert scheduler.size == scheduler.high_water == 2
        scheduler.abort()
        assert scheduler.reserve() is None
        token.cancel()
        await asyncio.wait_for(scheduler.close(), 1)
        assert observed == [("active", ReplyEnd.CANCELLED)]
        assert scheduler.size == scheduler.active == 0
        with pytest.raises(RuntimeError, match="already been released"):
            token.submit("late", deadline=deadline())
    finally:
        await scheduler.close()


@pytest.mark.asyncio
async def test_abort_releases_drain_without_requiring_close():
    async def execute(work):
        pytest.fail(f"aborted scheduler unexpectedly executed {work.value!r}")

    scheduler = ReplyScheduler(execute, capacity=1, concurrency=1)
    waiter = None
    try:
        assert scheduler.reserve() is not None
        waiter = asyncio.create_task(scheduler.drain())
        await asyncio.sleep(0)
        assert not waiter.done()
        scheduler.abort()
        assert scheduler.size == scheduler.active == 0
        await asyncio.wait_for(waiter, 0.05)
    finally:
        await scheduler.close()
        if waiter is not None:
            await asyncio.gather(waiter, return_exceptions=True)


@pytest.mark.asyncio
async def test_expiry_callback_can_admit_replacement_exactly_once():
    entered, release, callback_seen = asyncio.Event(), asyncio.Event(), asyncio.Event()
    observed = []
    admissions = []

    async def execute(work):
        if work.value == "running":
            entered.set()
            await release.wait()
        return work.progress.finish(ReplyEnd.FINISHED)

    scheduler = ReplyScheduler(
        execute,
        capacity=2,
        concurrency=1,
        on_finish=lambda value, outcome: observed.append((value, outcome.end)),
    )

    def completed(value, outcome):
        if value == "waiting" and outcome.end is ReplyEnd.EXPIRED and not admissions:
            admissions.append(scheduler.submit("replacement", deadline=deadline()))
            callback_seen.set()

    try:
        assert scheduler.submit("running", deadline=deadline()) is Admission.ACCEPTED
        await asyncio.wait_for(entered.wait(), 1)
        assert (
            scheduler.submit(
                "waiting",
                deadline=deadline(0.02),
                on_finish=completed,
            )
            is Admission.ACCEPTED
        )
        await asyncio.wait_for(callback_seen.wait(), 1)
        assert admissions == [Admission.ACCEPTED]
        release.set()
        await asyncio.wait_for(scheduler.drain(), 1)
        assert observed.count(("waiting", ReplyEnd.EXPIRED)) == 1
        assert ("replacement", ReplyEnd.FINISHED) in observed
    finally:
        release.set()
        await scheduler.close()


@pytest.mark.asyncio
async def test_mixed_addressed_and_timer_reservations_remain_bounded_without_waiter_tasks():
    capacity, concurrency, timer_count = 48, 4, 12
    release, all_active = asyncio.Event(), asyncio.Event()
    entered = []
    observed = []

    async def execute(work):
        entered.append(work.value)
        if len(entered) == concurrency:
            all_active.set()
        await release.wait()
        return work.progress.finish(ReplyEnd.FINISHED)

    scheduler = ReplyScheduler(
        execute,
        capacity=capacity,
        concurrency=concurrency,
        on_finish=lambda value, outcome: observed.append((value, outcome.end)),
    )
    try:
        for index in range(concurrency):
            assert scheduler.submit(f"addressed-{index}", deadline=deadline()) is Admission.ACCEPTED
        await asyncio.wait_for(all_active.wait(), 1)
        reserved = [scheduler.reserve() for _ in range(timer_count)]
        assert all(item is not None for item in reserved)
        assert scheduler.active == concurrency
        assert scheduler.size == concurrency + timer_count
        for index in range(capacity - concurrency - timer_count):
            assert (
                scheduler.submit(f"addressed-waiting-{index}", deadline=deadline())
                is Admission.ACCEPTED
            )
        assert scheduler.size == scheduler.high_water == capacity
        assert scheduler.submit("overflow", deadline=deadline()) is Admission.OVERLOADED
        for index, token in enumerate(reserved):
            assert token.submit(f"timer-{index}", deadline=deadline()) is Admission.ACCEPTED
        assert scheduler.size == scheduler.high_water == capacity
        assert scheduler.active == concurrency
        owned = [
            task
            for task in asyncio.all_tasks()
            if not task.done() and task.get_name() in ("reply-dispatch", "reply-session")
        ]
        assert len(owned) == concurrency + 1
        release.set()
        await asyncio.wait_for(scheduler.drain(), 2)
        assert len([item for item in observed if item[1] is ReplyEnd.FINISHED]) == capacity
        assert scheduler.active == scheduler.size == 0
    finally:
        release.set()
        await scheduler.close()


@pytest.mark.parametrize("method", ["reserve", "submit"])
async def test_expiration_observer_can_close_admission_synchronously(method):
    async def execute(work):
        pytest.fail("closed admission must not start work")

    scheduler = ReplyScheduler(execute, capacity=2, concurrency=1)
    try:
        assert (
            scheduler.submit("expired", deadline=deadline(), on_finish=lambda *_: scheduler.abort())
            is Admission.ACCEPTED
        )
        scheduler._pending[0].deadline = deadline(-1)
        if method == "reserve":
            assert scheduler.reserve() is None
        else:
            assert scheduler.submit("late", deadline=deadline()) is Admission.CLOSED
        assert scheduler.size == 0
        await asyncio.wait_for(scheduler.drain(), 1)
    finally:
        await scheduler.close()


async def test_reservation_remains_owned_during_reentrant_expiration_callbacks():
    observed, admissions = [], []

    async def execute(work):
        return work.progress.finish(ReplyEnd.FINISHED)

    scheduler = ReplyScheduler(
        execute,
        capacity=2,
        concurrency=1,
        on_finish=lambda value, outcome: observed.append((value, outcome.end)),
    )

    def expired(*_):
        admissions.append(scheduler.submit("replacement", deadline=deadline()))
        admissions.append(scheduler.submit("extra", deadline=deadline()))

    try:
        assert (
            scheduler.submit("expired", deadline=deadline(), on_finish=expired)
            is Admission.ACCEPTED
        )
        reserved = scheduler.reserve()
        assert reserved is not None
        scheduler._pending[0].deadline = deadline(-1)
        assert reserved.submit("claimed timer", deadline=deadline()) is Admission.ACCEPTED
        assert admissions == [Admission.ACCEPTED, Admission.OVERLOADED]
        assert scheduler.size == scheduler.high_water == 2
        await asyncio.wait_for(scheduler.drain(), 1)
        assert ("claimed timer", ReplyEnd.FINISHED) in observed
        assert ("replacement", ReplyEnd.FINISHED) in observed
    finally:
        await scheduler.close()
