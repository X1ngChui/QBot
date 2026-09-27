"""Own-echo correlation and task-local send continuation without QQ or DB."""

import _db as _test_db
import asyncio

import pytest

from _budget import fake_budget
from _db import clock, pool, tool_registry
from _fixtures import config, now_local
from qqbot.conversation.agent import AgentRun, SendResult, request_for_reply
from qqbot.conversation.member_numbers import MemberNumbers
from qqbot.conversation.state import ChatMsg, GroupState
from qqbot.conversation.tools import ToolCtx
from qqbot.delivery.observation import SelfEcho
from qqbot.delivery.segments import DiceSegment, TextSegment
from qqbot.delivery.service import GroupDelivery
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.providers.contracts import Message, ModelTurn, Role, ToolCall, ToolCallId


class People(MemberNumbers):
    async def learn(self, accounts):
        del accounts


class Session:
    def __init__(self, responses):
        self.responses = iter(responses)
        self.seen = []

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        return None

    async def start(self):
        return next(self.responses)

    async def continue_with(self, results, *, directive=None):
        self.seen.append((results, directive))
        return next(self.responses)


class Model:
    def __init__(self, responses):
        self.session = Session(responses)

    def open_session(self, request):
        del request
        return self.session


def msg(mid, uid, *, bot=False, text="plain"):
    return ChatMsg(
        msg_id=MessageId(mid),
        user_id=AccountId(uid),
        nickname="Fictional",
        text=text,
        ts=now_local(),
        is_bot=bot,
    )


def call(mid, name, args):
    return ToolCall(ToolCallId(mid), name, args)


def run(model, state, send, *, cfg=None):
    cfg = cfg or config().default
    first = state.recent[0]
    people = People(self_id="999")
    people.number(first.user_id, spoke=True)
    registry = tool_registry(cfg)
    return AgentRun(
        model=model,
        request=request_for_reply(
            (Message(Role.USER, "Fictional request"),),
            cfg,
            group_id=state.group_id,
            registry=registry,
        ),
        cfg=cfg,
        state=state,
        tool_context=ToolCtx(
            providers=object(),
            media=object(),
            by_pic={},
            people=people,
            identities=_test_db.identities,
            database=pool,
            clock=clock,
        ),
        people=people,
        lines={1: first},
        on_send=send,
        seen_messages={first.msg_id},
        budget=fake_budget(),
        registry=registry,
    )


@pytest.mark.asyncio
async def test_self_echo_is_scoped_to_group_and_account_and_cleans_waiters():
    group, other = GroupId("311"), GroupId("312")
    tracker = SelfEcho()
    try:
        own = msg("58", "999", bot=True, text="⟦骰子:4点⟧")
        tracker.publish("999", group, own)
        assert await tracker.wait("999", group, own.msg_id, timeout=0.01) is own
        assert await tracker.wait("999", other, own.msg_id, timeout=0.01) is None
        assert await tracker.wait("888", group, own.msg_id, timeout=0.01) is None
        assert tracker._waiters == {}
        pending = asyncio.create_task(tracker.wait("999", group, MessageId("60"), timeout=0.1))
        await asyncio.sleep(0)
        tracker.publish("999", group, msg("60", "888", bot=True))
        assert not pending.done()
        later = msg("60", "999", bot=True)
        tracker.publish("999", group, later)
        assert await pending is later
    finally:
        tracker.close()


@pytest.mark.asyncio
async def test_echo_wait_does_not_hold_group_send_lock():
    group = GroupId("311")
    delivery = GroupDelivery()

    class Bot:
        next_id = 0

        async def send_group_msg(self, *, group_id, message):
            del group_id, message
            self.next_id += 1
            return {"message_id": self.next_id}

    try:
        bot = Bot()
        first = await delivery.deliver_one(bot, group_id=group, segments=(TextSegment("First"),))
        waiting = asyncio.create_task(
            delivery.echo.wait("999", group, first.message_id, timeout=0.05)
        )
        second = await asyncio.wait_for(
            delivery.deliver_one(bot, group_id=group, segments=(TextSegment("Second"),)),
            timeout=0.02,
        )
        assert second.message_id == MessageId("2")
        assert await waiting is None
    finally:
        delivery.echo.close()


@pytest.mark.asyncio
async def test_confirmed_send_uses_own_echo_before_continuation():
    state = GroupState(
        group_id=GroupId("311"),
        recent=[msg("1", "101")],
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=clock.zone,
    )
    turn = ModelTurn(
        tool_calls=(call("dice", "send_message", '{"content":[{"type":"dice","data":{}}]}'),)
    )
    model = Model([turn, ModelTurn(tool_calls=(call("finish", "finish_reply", "{}"),))])

    async def send(draft, executed):
        del executed
        assert isinstance(draft.segments[0], DiceSegment)
        own = msg("2", "999", bot=True, text="⟦骰子:4点⟧")
        state.add(own)
        state.add(msg("3", "102", text="Another fictional request"))
        return SendResult("已确认发送并收到自身回显；平台显示：⟦骰子:4点⟧", own, True)

    outcome = await run(model, state, send).run()
    result, directive = model.session.seen[0]
    assert outcome.sent
    assert "4点" in result[0].output
    assert "#2" in result[0].output
    assert directive is not None
    assert "#3" in str(directive.prompt)
    assert "#2" not in str(directive.prompt)


@pytest.mark.asyncio
async def test_missing_echo_stops_before_continuation_or_resend():
    state = GroupState(
        group_id=GroupId("312"),
        recent=[msg("10", "101")],
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=clock.zone,
    )
    turn = ModelTurn(
        tool_calls=(call("dice", "send_message", '{"content":[{"type":"dice","data":{}}]}'),)
    )
    model = Model([turn])

    async def uncertain(*_):
        return SendResult("status unknown", confirmed=True)

    outcome = await run(model, state, uncertain).run()
    assert outcome.sent
    assert not model.session.seen


@pytest.mark.asyncio
async def test_mixed_send_and_schedule_executes_neither():
    state = GroupState(
        group_id=GroupId("312"),
        recent=[msg("11", "101")],
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=clock.zone,
    )
    model = Model(
        [
            ModelTurn(
                tool_calls=(
                    call("dice", "send_message", '{"content":[{"type":"dice","data":{}}]}'),
                    call("task", "schedule_task", "{}"),
                )
            ),
            ModelTurn(tool_calls=(call("finish", "finish_reply", "{}"),)),
        ]
    )
    calls = []

    async def unsent(*_):
        calls.append(True)
        return SendResult("sent")

    outcome = await run(model, state, unsent).run()
    assert not outcome.sent
    assert not calls
    assert all("均未执行" in item.output for item in model.session.seen[0][0])
