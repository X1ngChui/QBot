"""Reply ownership regressions: actual sends and the initial observation boundary."""

from unittest.mock import AsyncMock

import pytest

from _budget import fake_budget
import _db as _test_db
from _test_owners import fresh_tasks
from qqbot.services.members import MemberDirectory
from qqbot.conversation import engine
from qqbot.conversation.member_numbers import MemberNumbers
from qqbot.conversation.state import ChatMsg
from qqbot.conversation.state import GroupState
from qqbot.domain.reply import ReplyProgress
from qqbot.domain.ids import AccountId, GroupId, MessageId
from _fixtures import now_local


async def test_ack_survives_later_timeout(monkeypatch, bundle):
    async def generate(**kwargs):
        kwargs["progress"].confirm()
        raise TimeoutError("fictional response expired after acknowledgement")

    monkeypatch.setattr(engine, "generate", generate)
    outcome = await engine.respond(
        tasks=fresh_tasks(),
        bot=object(),
        st=GroupState(
            GroupId("311"),
            groups=_test_db.groups,
            archive=_test_db.archive,
            display_zone=_test_db.clock.zone,
        ),
        cfg=bundle.default,
        persona=bundle.persona_for(GroupId("311")),
        msg=None,
        providers=object(),
        media=object(),
        delivery=object(),
        directory=object(),
        budget=fake_budget(),
        members=MemberDirectory(),
        database=_test_db.pool,
        identities=_test_db.identities,
        evidence_store=_test_db.evidence,
        archive=_test_db.archive,
        clock=_test_db.clock,
        prompts=_test_db.test_bundle().prompts,
    )
    assert outcome.sent


async def test_arrival_during_media_wait_is_not_premarked_seen(monkeypatch, bundle):
    def message(mid):
        return ChatMsg(
            msg_id=MessageId(mid),
            user_id=AccountId("101"),
            nickname="Fictional",
            text="Fictional request",
            ts=now_local(),
        )

    history, asked, intervening = (message(str(n)) for n in range(1, 4))
    state = GroupState(
        GroupId("311"),
        recent=[history, asked, intervening],
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    monkeypatch.setattr(MemberDirectory, "relabel", AsyncMock())
    monkeypatch.setattr(MemberNumbers, "learn", AsyncMock())
    monkeypatch.setattr(engine.retrieval, "gather", AsyncMock(return_value=[]))
    monkeypatch.setattr(engine.retrieval, "group_knowledge", AsyncMock(return_value=""))
    monkeypatch.setattr(_test_db.evidence, "evidence_for", AsyncMock(return_value={}))
    monkeypatch.setattr(engine.prompt, "assemble", lambda **kwargs: ())

    class Captured(Exception):
        pass

    def capture(**kwargs):
        assert intervening.msg_id not in kwargs["seen_messages"]
        assert asked.msg_id in kwargs["seen_messages"]
        raise Captured

    monkeypatch.setattr(engine.agent, "AgentRun", capture)

    class Bot:
        self_id = "999"

    class Providers:
        text = object()

    with pytest.raises(Captured):
        await engine.generate(
            tasks=fresh_tasks(),
            bot=Bot(),
            st=state,
            cfg=bundle.default,
            persona=bundle.persona_for(state.group_id),
            msg=asked,
            providers=Providers(),
            media=object(),
            directory=object(),
            delivery=object(),
            window=[history],
            progress=ReplyProgress(),
            budget=fake_budget(),
            members=MemberDirectory(),
            database=_test_db.pool,
            identities=_test_db.identities,
            evidence_store=_test_db.evidence,
            archive=_test_db.archive,
            clock=_test_db.clock,
            prompts=_test_db.test_bundle().prompts,
        )
