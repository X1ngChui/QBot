"""Free backends and adversarial tool output cannot make a session unbounded."""

import json
from types import SimpleNamespace
from unittest.mock import AsyncMock

from hypothesis import given, strategies as st
import pytest

from _budget import fake_budget
import _db as _test_db
from qqbot.conversation import agent, tools
from qqbot.conversation.fuel import (
    MAX_ARGUMENT_BYTES,
    MAX_CALLS_PER_TURN,
    MAX_MODEL_TURNS,
    MAX_RESULT_BYTES,
    MAX_WIRE_CALLS,
    SessionFuel,
)
from qqbot.conversation.state import GroupState
from qqbot.domain.ids import GroupId
from qqbot.providers.contracts import (
    CallContext,
    GenerationPolicy,
    ModelRequest,
    ModelTurn,
    StoredImage,
    ToolCall,
    ToolCallId,
)
from qqbot.services.budget import Scope
from qqbot.util import defang, sysmark


class EndlessSession:
    def __init__(self, width=1):
        self.turns = 0
        self.width = width
        self.closed = False

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        self.closed = True

    async def start(self):
        self.turns += 1
        return ModelTurn(
            tool_calls=tuple(
                ToolCall(
                    ToolCallId(f"c-{self.turns}-{i}"),
                    "web_search",
                    json.dumps({"query": f"fictional-{self.turns}-{i}"}),
                )
                for i in range(self.width)
            )
        )

    async def continue_with(self, results, *, directive=None):
        return await self.start()


def run(bundle, session):
    state = GroupState(
        GroupId("311"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    return agent.AgentRun(
        budget=fake_budget(),
        model=SimpleNamespace(open_session=lambda request: session),
        request=ModelRequest((), (), GenerationPolicy("fictional"), CallContext()),
        cfg=bundle.default,
        state=state,
        tool_context=tools.ToolCtx(
            object(),
            object(),
            identities=_test_db.identities,
            database=_test_db.pool,
            clock=_test_db.clock,
        ),
        people=object(),
        lines={},
        on_send=AsyncMock(),
        seen_messages=set(),
        registry=_test_db.tool_registry(bundle.default),
    )


async def test_free_endless_model_uses_no_more_than_total_turn_fuel(bundle, monkeypatch):
    session = EndlessSession()
    execute = AsyncMock(return_value="Fictional result")
    monkeypatch.setattr(tools, "execute", execute)
    instance = run(bundle, session)
    await instance.run()
    assert session.turns == MAX_MODEL_TURNS
    assert execute.await_count < MAX_MODEL_TURNS
    assert session.closed


async def test_total_tool_fuel_is_independent_of_round_count(bundle, monkeypatch):
    session = EndlessSession(MAX_CALLS_PER_TURN)
    execute = AsyncMock(return_value="Fictional result")
    monkeypatch.setattr(tools, "execute", execute)
    instance = run(bundle, session)
    instance._fuel.calls_left = 10
    await instance.run()
    assert execute.await_count <= 10
    assert session.turns <= 3
    assert instance._fuel.calls_left == 0


@pytest.mark.parametrize("width", [MAX_WIRE_CALLS + 1, 1000])
async def test_huge_tool_turn_has_no_side_effects_or_debug_growth(bundle, monkeypatch, width):
    execute = AsyncMock()
    monkeypatch.setattr(tools, "execute", execute)
    instance = run(bundle, EndlessSession(width))
    await instance.run()
    execute.assert_not_awaited()
    assert instance._debug_items == []


async def test_oversized_arguments_are_rejected_before_parsing(bundle, monkeypatch):
    execute = AsyncMock()
    monkeypatch.setattr(tools, "execute", execute)
    session = EndlessSession()
    session.start = AsyncMock(
        return_value=ModelTurn(
            tool_calls=(ToolCall(ToolCallId("huge"), "web_search", "x" * (MAX_ARGUMENT_BYTES + 1)),)
        )
    )
    await run(bundle, session).run()
    execute.assert_not_awaited()


@given(st.lists(st.text(max_size=500), max_size=20), st.integers(min_value=4, max_value=500))
def test_retained_results_are_utf8_bounded_and_keep_complete_markers(values, limit):
    fuel = SessionFuel(result_bytes_left=limit)
    results = [fuel.retain(sysmark(defang(value))) for value in values]
    assert sum(len(result.encode("utf-8")) for result in results) <= limit
    assert all(result.count("⟦") == result.count("⟧") for result in results)


async def test_large_result_cannot_expand_replay_or_evidence_unboundedly(bundle, monkeypatch):
    monkeypatch.setattr(tools, "execute", AsyncMock(return_value="大" * 100_000))
    instance = run(bundle, EndlessSession())
    await instance.run()
    assert sum(len(item.output.encode("utf-8")) for item in instance.executed) <= MAX_RESULT_BYTES
    assert instance._fuel.result_bytes_left < 4


async def test_omitted_attachments_are_not_reported_as_images_the_model_saw(bundle, monkeypatch):
    answer = tools.Attachment("Fictional images", (StoredImage("p", "a"), StoredImage("p", "b")))
    monkeypatch.setattr(tools, "execute", AsyncMock(return_value=answer))
    instance = run(bundle, EndlessSession())
    instance._fuel.images_left = 1
    results, exhausted = await instance._execute_round(
        (ToolCall(ToolCallId("images"), "open_images", json.dumps({"ns": [1, 2]})),),
        spend=Scope(1),
    )
    assert exhausted and results[0].output == agent.QUOTA_NOTE
    assert not instance.executed[0].verified
