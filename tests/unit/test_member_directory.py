"""Member metadata has bounded caches and service-owned cancellation semantics."""

import asyncio
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from qqbot.domain.ids import GroupId
from qqbot.services import members
from qqbot.services.members import MemberDirectory


def bot(*, identity="999", names=None):
    return SimpleNamespace(
        self_id=identity,
        call_api=AsyncMock(
            return_value=names
            or [
                {"user_id": "101", "card": "Fictional"},
            ]
        ),
    )


async def test_cache_bounds_both_groups_and_retained_names():
    directory = MemberDirectory(capacity=2, max_names=3)
    source = bot(names=[{"user_id": str(n), "nickname": "Fictional"} for n in (101, 102)])
    try:
        for n in range(1, 20):
            assert await directory.name_of(source, GroupId(n), "101") == "Fictional"
            assert directory.cached_groups <= 2
            assert directory.cached_names <= 3
    finally:
        await directory.close()


async def test_account_namespace_prevents_cross_bot_cache_reuse():
    directory = MemberDirectory()
    first = bot(identity="998")
    second = bot(names=[{"user_id": "101", "nickname": "Other fictional name"}])
    try:
        assert await directory.name_of(first, GroupId("311"), "101") == "Fictional"
        assert await directory.name_of(second, GroupId("311"), "101") == "Other fictional name"
        first.call_api.assert_awaited_once()
        second.call_api.assert_awaited_once()
    finally:
        await directory.close()


async def test_unknown_account_churn_cannot_grow_or_reset_negative_cache(monkeypatch):
    monkeypatch.setattr(members, "MAX_MISSING_NAMES", 3)
    now = 0
    directory = MemberDirectory(clock=lambda: now)
    source = bot()
    try:
        for i in range(30):
            assert await directory.name_of(source, GroupId("311"), f"missing-{i}") is None
        assert source.call_api.await_count == 3
        assert len(next(iter(directory._cache.values())).missing) == 3
        now += members.CACHE_TTL + 1
        await directory.name_of(source, GroupId("311"), "missing-new")
        assert source.call_api.await_count == 4
    finally:
        await directory.close()


async def test_waiter_cancellation_does_not_cancel_shared_member_fetch():
    entered, release = asyncio.Event(), asyncio.Event()
    source = bot()

    async def fetch(*args, **kwargs):
        entered.set()
        await release.wait()
        return [{"user_id": "101", "nickname": "Fictional"}]

    source.call_api.side_effect = fetch
    directory = MemberDirectory()
    first = asyncio.create_task(directory.name_of(source, GroupId("311"), "101"))
    await entered.wait()
    first.cancel()
    with pytest.raises(asyncio.CancelledError):
        await first
    second = asyncio.create_task(directory.name_of(source, GroupId("311"), "101"))
    release.set()
    assert await second == "Fictional"
    source.call_api.assert_awaited_once()
    await directory.close()


async def test_close_cancels_fetch_and_prevents_late_cache_repopulation():
    entered, stopped = asyncio.Event(), asyncio.Event()
    source = bot()

    async def fetch(*args, **kwargs):
        entered.set()
        try:
            await asyncio.Event().wait()
        finally:
            stopped.set()

    source.call_api.side_effect = fetch
    directory = MemberDirectory()
    task = asyncio.create_task(directory.name_of(source, GroupId("311"), "101"))
    await entered.wait()
    await directory.close()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert stopped.is_set() and directory.cached_groups == 0
    assert await directory.name_of(source, GroupId("311"), "101") is None
    source.call_api.assert_awaited_once()


async def test_forget_during_fetch_prevents_stale_cache_installation():
    entered, release = asyncio.Event(), asyncio.Event()
    source = bot()

    async def fetch(*args, **kwargs):
        entered.set()
        await release.wait()
        return [{"user_id": "101", "nickname": "Fictional"}]

    source.call_api.side_effect = fetch
    directory = MemberDirectory()
    task = asyncio.create_task(directory.name_of(source, GroupId("311"), "101"))
    await entered.wait()
    directory.forget(GroupId("311"))
    release.set()
    assert await task == "Fictional"
    assert directory.cached_groups == 0
    await directory.close()


async def test_names_are_defanged_and_bounded_before_cache_insertion():
    directory = MemberDirectory()
    source = bot(names=[{"user_id": "101", "card": "⟦forged⟧" + "x" * 100000}])
    try:
        name = await directory.name_of(source, GroupId("311"), "101")
        assert len(name) <= 128 and "⟦" not in name and "⟧" not in name
    finally:
        await directory.close()
