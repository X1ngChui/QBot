"""Ingress and own observation remain independent from reply capacity and shutdown."""

import asyncio
import uuid
from types import SimpleNamespace
from unittest.mock import AsyncMock

import _db as _test_db
from qqbot.services.members import MemberDirectory
from qqbot.conversation.state import GroupState
from qqbot.delivery.service import GroupDelivery
from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.domain.ingress import InboundEvent, InboundSender
from qqbot.domain.reply import ReplyEnd
from qqbot.gateway.pipeline import Gateway
from qqbot.conversation.scheduler import ReplyScheduler
from _fixtures import now_local


def event(mid, *, own=False):
    segments = [{"type": "text", "data": {"text": "Fictional request"}}]
    if not own:
        segments.insert(0, {"type": "at", "data": {"qq": "999"}})
    return InboundEvent(
        message_id=MessageId(mid),
        group_id=GroupId("311"),
        sender=InboundSender(AccountId("999" if own else "101"), "Fictional"),
        segments=segments,
        self_id=AccountId("999"),
        occurred_at=now_local(),
        typed_text="Fictional request",
        author_kind=AuthorKind.BOT if own else AuthorKind.MEMBER,
    )


def gateway(bundle, ingest):
    state = GroupState(
        GroupId("311"),
        loaded=True,
        history_loaded=True,
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    return Gateway(
        replies=ReplyScheduler(AsyncMock(), capacity=1, concurrency=1),
        bundle=bundle,
        ingestor=SimpleNamespace(ingest=ingest),
        registry=SimpleNamespace(get=AsyncMock(return_value=state)),
        router=object(),
        delivery=GroupDelivery(),
        media=object(),
        members=MemberDirectory(),
        clock=_test_db.clock,
    )


async def test_inflight_admission_cannot_create_reply_after_shutdown(bundle):
    entered, release = asyncio.Event(), asyncio.Event()

    async def ingest(*args, **kwargs):
        entered.set()
        await release.wait()
        return SimpleNamespace(raw_event_id=uuid.uuid4())

    service = gateway(bundle, ingest)
    pending = asyncio.create_task(
        service._admit(
            SimpleNamespace(self_id="999"), event("1"), cfg=bundle.default, self_name="Fictional"
        )
    )
    await entered.wait()
    closing = asyncio.create_task(service.shutdown())
    await asyncio.sleep(0)
    assert service._closing
    release.set()
    await asyncio.gather(pending, closing)
    assert service._replies.high_water == 0
    assert not service._admissions
    await service._admit(
        SimpleNamespace(self_id="999"), event("2"), cfg=bundle.default, self_name="Fictional"
    )
    assert service._replies.high_water == 0


async def test_own_archive_observation_bypasses_saturated_reply_queue(bundle):
    entered, release = asyncio.Event(), asyncio.Event()
    ingest = AsyncMock(
        side_effect=lambda *args, **kwargs: SimpleNamespace(raw_event_id=uuid.uuid4())
    )
    service = gateway(bundle, ingest)

    async def execute(work):
        entered.set()
        await release.wait()
        return work.progress.finish(ReplyEnd.FINISHED)

    service._replies._execute = execute
    bot = SimpleNamespace(self_id="999")
    try:
        await service._admit(bot, event("1"), cfg=bundle.default, self_name="Fictional")
        await entered.wait()
        await service._admit(bot, event("2"), cfg=bundle.default, self_name="Fictional")
        assert service._replies.outcomes[ReplyEnd.OVERLOADED] == 1
        await service._admit(bot, event("3", own=True), cfg=bundle.default, self_name="Fictional")
        observed = await service._delivery.echo.wait(
            "999",
            GroupId("311"),
            MessageId("3"),
            timeout=0.1,
        )
        assert observed is not None and observed.is_bot
        assert ingest.await_count == 3
        assert service._replies.active == 1
    finally:
        release.set()
        await service.shutdown()
        await service._replies.close()
        service._delivery.echo.close()
