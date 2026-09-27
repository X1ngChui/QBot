"""Message media admission and result retention have explicit owners and bounds."""

import asyncio
from types import SimpleNamespace
from unittest.mock import AsyncMock
import uuid

from _budget import fake_budget
import _db as _test_db
from qqbot.services.members import MemberDirectory
from qqbot.conversation.state import ChatMsg
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.gateway.segments import parse_segments
from qqbot.media.ratelimit import SlidingWindow
from qqbot.media.result import Resolution
from qqbot.media.service import MediaProcessor
from qqbot.media.coordinator import MediaCoordinator
from _fixtures import now_local


def message():
    parsed = parse_segments(
        [
            {"type": "image", "data": {"file": "a" * 32}},
            {"type": "record", "data": {"file": "fictional.wav"}},
        ],
        "999",
    )
    msg = ChatMsg(
        msg_id=MessageId("1"),
        user_id=AccountId("101"),
        nickname="Fictional",
        text=parsed.render(),
        ts=now_local(),
        raw_event_id=uuid.uuid4(),
    )
    return msg, parsed


async def test_completed_reference_is_not_repeated_when_another_reference_retries(bundle):
    processor = MediaProcessor(
        SimpleNamespace(),
        SimpleNamespace(),
        budget=fake_budget(),
        members=MemberDirectory(),
        cache=_test_db.media_cache,
        prompts=_test_db.test_bundle().prompts,
    )
    processor.resolve_picture = AsyncMock(
        side_effect=[
            Resolution(retryable=True),
            Resolution("⟦图片:fictional⟧"),
        ]
    )
    processor.transcribe = AsyncMock(return_value=Resolution("⟦语音:fictional⟧"))
    _, parsed = message()
    try:
        result = await processor.resolve(
            parsed, bot=object(), group_id=GroupId("1"), cfg=bundle.default
        )
        assert not processor.settled(parsed, result)
        result = await processor.resolve(
            parsed,
            bot=object(),
            group_id=GroupId("1"),
            cfg=bundle.default,
            previous=result,
        )
        assert processor.settled(parsed, result)
        assert processor.transcribe.await_count == 1
        assert processor.resolve_picture.await_count == 2
    finally:
        await processor.close()


async def test_coordinator_capacity_and_close_prevent_unowned_tasks(bundle):
    entered = asyncio.Event()

    async def resolve(*args, **kwargs):
        entered.set()
        await asyncio.Event().wait()

    processor = SimpleNamespace(resolve=resolve)
    coordinator = MediaCoordinator(
        processor, capacity=1, budget=fake_budget(), archive=_test_db.archive
    )
    first, parsed = message()
    second, _ = message()
    kwargs = {"bot": object(), "group_id": GroupId("1"), "cfg": bundle.default}
    ticket = coordinator.admit(first.raw_event_id, parsed, first, **kwargs)
    await entered.wait()
    assert coordinator.admit(second.raw_event_id, parsed, second, **kwargs) is None
    assert coordinator.high_water == 1 and coordinator.overloaded == 1
    task = ticket.task
    await coordinator.close(timeout=0)
    assert task.done() and task.cancelled()
    assert coordinator.admit(second.raw_event_id, parsed, second, **kwargs) is None
    assert not coordinator._tickets


def test_rate_registry_reclaims_idle_entries_but_never_evicts_live_quotas():
    table = {GroupId(str(i)): SlidingWindow(1) for i in range(1, 1025)}
    for window in table.values():
        assert window.take()
    assert not MediaProcessor._window(table, GroupId("2001"), 1).take()
    assert len(table) == 1024
    assert not table[GroupId("1")].take()
    table[GroupId("1")] = SlidingWindow(1)
    assert MediaProcessor._window(table, GroupId("2001"), 1).take()
    assert len(table) == 1024
