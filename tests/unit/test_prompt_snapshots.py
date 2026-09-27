"""Model-visible transcript values never change behind stable line/image numbers."""

import asyncio
from dataclasses import FrozenInstanceError
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from _budget import fake_budget
import _db as _test_db
from qqbot.conversation.agent import AgentRun
from qqbot.conversation.snapshot import (
    CONTINUATION_MESSAGES,
    ContinuationSnapshot,
    MessageSnapshot,
    PromptSnapshot,
)
from qqbot.conversation.state import ChatMsg, GroupState
from qqbot.conversation.tools import ToolCtx
from qqbot.domain.ids import GroupId
from qqbot.gateway.segments import ImageRef
from qqbot.providers.contracts import CallContext, GenerationPolicy, ModelRequest
from _fixtures import now_local


def message(identifier, *, own=False):
    return ChatMsg(
        msg_id=str(identifier),
        user_id="999" if own else "101",
        nickname="Fictional",
        text="Original",
        ts=now_local(),
        is_bot=own,
    )


def test_snapshot_is_deeply_detached_from_media_and_name_patches():
    state = GroupState(
        GroupId("311"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    old, asked = message(1), message(2)
    old.image_refs = [ImageRef(key="first", url="https://example.invalid/first")]
    old.mentions = [("102", "Fictional other")]
    old.text = "⟦图片⟧"
    state.add(old)
    state.add(asked)
    snapshot = PromptSnapshot.capture([old], asked, cursor=state.arrival_seq)
    rendered = snapshot.history[0].render(pic_nums=snapshot.image_numbers["1"])
    old.text = "Late media patch"
    old.nickname = "Late rename"
    old.mentions.append(("103", "New mention"))
    old.image_refs[0].url = "https://example.invalid/replaced"
    old.image_refs.append(ImageRef(key="second"))
    assert snapshot.history[0].render(pic_nums=snapshot.image_numbers["1"]) == rendered
    assert snapshot.pictures[1][0].image_refs[0].url == "https://example.invalid/first"
    assert len(snapshot.history[0].mentions) == 1
    with pytest.raises(FrozenInstanceError):
        snapshot.history[0].text = "cannot change"
    with pytest.raises(FrozenInstanceError):
        snapshot.history[0].image_refs[0].key = "cannot change"
    with pytest.raises(TypeError):
        snapshot.numbers["new"] = 99


def test_continuation_records_eviction_and_limits_each_projection():
    state = GroupState(
        GroupId("311"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    for index in range(500):
        state.add(message(index))
    snapshot = ContinuationSnapshot.capture(state.recent, 1)
    assert snapshot.gap
    assert len(snapshot.messages) == CONTINUATION_MESSAGES
    assert snapshot.cursor == 500
    assert not ContinuationSnapshot.capture(state.recent, snapshot.cursor).messages


def test_observed_send_survives_live_window_eviction_without_hiding_the_gap():
    state = GroupState(
        GroupId("311"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    own = message("own", own=True)
    state.add(own)
    for index in range(500):
        state.add(message(index))
    snapshot = ContinuationSnapshot.capture(state.recent, 0, own=own)
    assert snapshot.gap and snapshot.messages[0].msg_id == own.msg_id
    assert len(snapshot.messages) == CONTINUATION_MESSAGES + 1
    assert snapshot.cursor == state.arrival_seq


async def test_arrival_snapshot_precedes_identity_io_and_advances_only_its_cursor(bundle):
    state = GroupState(
        GroupId("311"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    first, second, third = message(1), message(2), message(3)
    state.add(first)
    state.add(second)
    entered, release = asyncio.Event(), asyncio.Event()

    async def learn(accounts):
        entered.set()
        await release.wait()

    people = SimpleNamespace(learn=learn, number=lambda *args, **kwargs: 1)
    run = AgentRun(
        budget=fake_budget(),
        model=object(),
        request=ModelRequest((), (), GenerationPolicy("fictional"), CallContext()),
        cfg=bundle.default,
        state=state,
        tool_context=ToolCtx(
            object(),
            object(),
            identities=_test_db.identities,
            database=_test_db.pool,
            clock=_test_db.clock,
        ),
        people=people,
        lines={1: first},
        on_send=AsyncMock(),
        seen_messages={first.msg_id},
        registry=_test_db.tool_registry(bundle.default),
    )
    projection = asyncio.create_task(run._arrivals())
    await entered.wait()
    second.text = "Late mutation"
    state.add(third)
    release.set()
    result = await projection
    assert "Original" in result[0].content and "Late mutation" not in result[0].content
    assert isinstance(run._lines[2], MessageSnapshot)
    assert run._cursor == 2 and third.msg_id not in run._seen_messages
    await run._arrivals()
    assert run._cursor == 3 and run._lines[3].msg_id == third.msg_id


async def test_gap_is_explicit_in_the_model_continuation(bundle):
    state = GroupState(
        GroupId("311"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    for index in range(500):
        state.add(message(index))
    run = AgentRun(
        budget=fake_budget(),
        model=object(),
        request=ModelRequest((), (), GenerationPolicy("fictional"), CallContext()),
        cfg=bundle.default,
        state=state,
        tool_context=ToolCtx(
            object(),
            object(),
            identities=_test_db.identities,
            database=_test_db.pool,
            clock=_test_db.clock,
        ),
        people=SimpleNamespace(learn=AsyncMock(), number=lambda *args, **kwargs: 1),
        lines={},
        on_send=AsyncMock(),
        seen_messages=set(),
        initial_cursor=1,
        registry=_test_db.tool_registry(bundle.default),
    )
    result = await run._arrivals()
    assert "⟦context gap:" in result[0].content
    assert len(run._lines) == CONTINUATION_MESSAGES
