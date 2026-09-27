"""Shared media work has one owner and a finite number of active tasks."""

import asyncio

import pytest

from qqbot.concurrency import Rejection, SharedWork, WorkRejected


async def test_waiter_cancellation_keeps_the_owner_and_shares_the_result():
    entered, release = asyncio.Event(), asyncio.Event()
    work = SharedWork(capacity=1, timeout=1)
    calls = 0

    async def execute():
        nonlocal calls
        calls += 1
        entered.set()
        await release.wait()
        return "result"

    first = asyncio.create_task(work.run("image", execute))
    await entered.wait()
    first.cancel()
    with pytest.raises(asyncio.CancelledError):
        await first
    assert work.size == 1
    second = asyncio.create_task(work.run("image", execute))
    await asyncio.sleep(0)
    release.set()
    assert await second == "result"
    assert calls == 1 and work.size == 0
    await work.close()


async def test_full_registry_rejects_before_starting_work_but_still_joins_existing_keys():
    entered, release = asyncio.Event(), asyncio.Event()
    work = SharedWork(capacity=1, timeout=1)

    async def execute():
        entered.set()
        await release.wait()
        return 1

    first = asyncio.create_task(work.run("one", execute))
    await entered.wait()
    with pytest.raises(WorkRejected) as rejected:
        await work.run("two", execute)
    assert rejected.value.reason is Rejection.FULL
    second = asyncio.create_task(work.run("one", execute))
    await asyncio.sleep(0)
    release.set()
    assert await asyncio.gather(first, second) == [1, 1]
    assert work.high_water == 1
    await work.close()


async def test_close_joins_owners_and_permanently_closes_admission():
    entered, finished = asyncio.Event(), asyncio.Event()
    work = SharedWork(capacity=1, timeout=1)

    async def execute():
        entered.set()
        try:
            await asyncio.Event().wait()
        finally:
            finished.set()

    waiter = asyncio.create_task(work.run("one", execute))
    await entered.wait()
    await work.close()
    assert finished.is_set() and work.size == 0
    with pytest.raises(asyncio.CancelledError):
        await waiter
    with pytest.raises(WorkRejected) as rejected:
        await work.run("one", execute)
    assert rejected.value.reason is Rejection.CLOSED
    await work.close()


async def test_owner_deadline_releases_the_slot_after_all_waiters_leave():
    entered, finished = asyncio.Event(), asyncio.Event()
    work = SharedWork(capacity=1, timeout=0.02)

    async def execute():
        entered.set()
        try:
            await asyncio.Event().wait()
        finally:
            finished.set()

    waiter = asyncio.create_task(work.run("one", execute))
    await entered.wait()
    with pytest.raises(TimeoutError):
        await waiter
    assert finished.is_set() and work.size == 0
    assert await work.run("two", lambda: asyncio.sleep(0, result=2)) == 2
    await work.close()


async def test_nested_work_cannot_deadlock_a_full_registry():
    work = SharedWork(capacity=1, timeout=1)

    async def parent():
        return await work.run("child", lambda: asyncio.sleep(0))

    with pytest.raises(WorkRejected) as rejected:
        await work.run("parent", parent)
    assert rejected.value.reason is Rejection.FULL
    assert work.size == 0
    await work.close()
