"""Every requested tool call executes within existing fuel and budget bounds."""

from itertools import count
import json
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from _budget import fake_budget
import _db as _test_db
from qqbot.conversation import agent, tools
from qqbot.conversation.fuel import MAX_MODEL_TURNS
from qqbot.conversation.member_numbers import MemberNumbers
from qqbot.conversation.state import GroupState
from qqbot.domain.ids import GroupId
from qqbot.providers.contracts import Message, ModelTurn, Role, ToolCall, ToolCallId, ToolResult
from qqbot.providers.deepseek import DeepSeekResponsesCodec
from qqbot.providers.openai_responses import ResponsesCodec, ResponsesExecutor, ResponsesTextSession


class Session:
    def __init__(self, turns):
        self.turns = iter(turns)
        self.request = None
        self.started = 0
        self.seen = []
        self.closed = False

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        self.closed = True

    async def start(self):
        self.started += 1
        return next(self.turns)

    async def continue_with(self, results, *, directive=None):
        self.seen.append((results, directive))
        return await self.start()


def turn(call_id, name, arguments="{}"):
    return ModelTurn(tool_calls=(ToolCall(ToolCallId(call_id), name, arguments),))


def run(bundle, session=None, *, budget=None, model=None):
    cfg = bundle.default
    registry = tools.tool_registry(cfg, prompts=bundle.prompts)
    state = GroupState(
        GroupId("311"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    people = MemberNumbers(self_id="999")
    prompt = (
        Message(Role.SYSTEM, "Fictional system instructions"),
        Message(Role.USER, "Fictional original goal: manage tasks and verify each change"),
        ToolCall(ToolCallId("prior-search"), "web_search", '{"query":"prior fictional progress"}'),
        ToolResult(ToolCallId("prior-search"), "Fictional prior search result"),
    )
    request = agent.request_for_reply(prompt, cfg, group_id=state.group_id, registry=registry)

    def open_session(received):
        session.request = received
        return session

    return agent.AgentRun(
        model=model or SimpleNamespace(open_session=open_session),
        request=request,
        cfg=cfg,
        registry=registry,
        budget=budget or fake_budget(),
        state=state,
        tool_context=tools.ToolCtx(
            object(),
            object(),
            initiator="101",
            people=people,
            identities=_test_db.identities,
            database=_test_db.pool,
            clock=_test_db.clock,
        ),
        people=people,
        lines={},
        on_send=AsyncMock(),
        seen_messages=set(),
    )


def assert_retained_progress(instance, session):
    assert session.request is instance._request
    prompt = session.request.prompt
    assert instance._debug_items[: len(prompt)] == list(prompt)
    results = [result for batch, _ in session.seen for result in batch]
    assert [
        item for item in instance._debug_items[len(prompt) :] if isinstance(item, ToolResult)
    ] == results
    assert all(directive is None for _, directive in session.seen)


async def test_listing_returns_fresh_results_after_create_and_cancel(bundle, monkeypatch):
    old_id = "00000000-0000-0000-0000-000000000001"
    new_id = "00000000-0000-0000-0000-000000000002"
    tasks = {old_id: "Fictional old task"}
    session = Session(
        [
            turn("old-list", "list_scheduled_tasks"),
            turn("create", "schedule_task", '{"intent":"Fictional new task","delay_seconds":600}'),
            turn("new-list", "list_scheduled_tasks"),
            turn("cancel", "cancel_scheduled_task", json.dumps({"id": old_id})),
            turn("final-list", "list_scheduled_tasks"),
            turn("finish", tools.FINISH),
        ]
    )

    async def execute(call, **kwargs):
        assert kwargs["ctx"].initiator == "101"
        if call.name == "list_scheduled_tasks":
            assert json.loads(call.arguments) == {}
            return json.dumps(tasks, sort_keys=True)
        if call.name == "schedule_task":
            tasks[new_id] = json.loads(call.arguments)["intent"]
            return "Created fictional task"
        assert call.name == "cancel_scheduled_task"
        del tasks[json.loads(call.arguments)["id"]]
        return "Cancelled fictional task"

    dispatch = AsyncMock(side_effect=execute)
    monkeypatch.setattr(tools, "execute", dispatch)
    instance = run(bundle, session)
    outcome = await instance.run()

    assert [item.args[0].name for item in dispatch.await_args_list] == [
        "list_scheduled_tasks",
        "schedule_task",
        "list_scheduled_tasks",
        "cancel_scheduled_task",
        "list_scheduled_tasks",
    ]
    results = [results[0] for results, _ in session.seen]
    assert [result.call_id for result in results] == [
        "old-list",
        "create",
        "new-list",
        "cancel",
        "final-list",
    ]
    assert [json.loads(results[index].output) for index in (0, 2, 4)] == [
        {old_id: "Fictional old task"},
        {old_id: "Fictional old task", new_id: "Fictional new task"},
        {new_id: "Fictional new task"},
    ]
    assert len(outcome.executed) == 5
    assert all(item.verified for item in outcome.executed)
    assert_retained_progress(instance, session)
    assert not outcome.sent and session.closed


@pytest.mark.parametrize("same_round", [False, True])
@pytest.mark.parametrize(
    "name, arguments",
    [
        ("list_scheduled_tasks", "{}"),
        ("schedule_task", '{"intent":"Fictional task","delay_seconds":600}'),
        ("cancel_scheduled_task", '{"id":"00000000-0000-0000-0000-000000000001"}'),
        ("web_search", '{"query":"fictional"}'),
        ("search_history", '{"query":"fictional"}'),
    ],
)
async def test_each_identical_call_executes_and_supplies_its_own_result(
    bundle, monkeypatch, name, arguments, same_round
):
    calls = [turn("first", name, arguments), turn("repeat", name, arguments)]
    if same_round:
        calls = [ModelTurn(tool_calls=tuple(item.tool_calls[0] for item in calls))]
    session = Session([*calls, turn("finish", tools.FINISH)])
    dispatch = AsyncMock(side_effect=["Fictional first result", "Fictional second result"])
    monkeypatch.setattr(tools, "execute", dispatch)
    instance = run(bundle, session)
    outcome = await instance.run()

    assert dispatch.await_count == 2
    assert [item.args[0].arguments for item in dispatch.await_args_list] == [arguments, arguments]
    results = [result for batch, _ in session.seen for result in batch]
    assert [(result.call_id, result.output) for result in results] == [
        ("first", "Fictional first result"),
        ("repeat", "Fictional second result"),
    ]
    assert [(item.name, item.output) for item in outcome.executed] == [
        (name, "Fictional first result"),
        (name, "Fictional second result"),
    ]
    assert all(item.verified for item in outcome.executed)
    assert_retained_progress(instance, session)


@pytest.mark.parametrize("codec", [ResponsesCodec(), DeepSeekResponsesCodec()])
async def test_real_provider_session_replays_original_goal_and_every_call_result(
    bundle, monkeypatch, codec
):
    names = [
        "list_scheduled_tasks",
        "schedule_task",
        "list_scheduled_tasks",
        "schedule_task",
        "web_search",
        "web_search",
        "list_scheduled_tasks",
        tools.FINISH,
    ]
    arguments = {
        "list_scheduled_tasks": "{}",
        "schedule_task": '{"intent":"Fictional task","delay_seconds":600}',
        "web_search": '{"query":"fictional"}',
        tools.FINISH: "{}",
    }
    turns = [turn(f"call-{index}", name, arguments[name]) for index, name in enumerate(names)]

    class Executor:
        _request = ResponsesExecutor._request

        def __init__(self):
            self.codec = codec
            self.inputs = []
            self.outputs = []

        async def complete(self, input_items, tools, **kwargs):
            index = len(self.inputs)
            self.inputs.append(list(input_items))
            output = (
                {
                    "type": "reasoning",
                    "id": f"reasoning-{index}",
                    "encrypted_content": f"opaque-{index}",
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": f"Fictional progress {index}"}],
                },
                *codec.encode_items(turns[index].tool_calls),
            )
            self.outputs.append(output)
            return SimpleNamespace(wire=SimpleNamespace(output=output), turn=turns[index])

    executor = Executor()

    class Model:
        def open_session(self, request):
            self.request = request
            self.session = ResponsesTextSession(executor, request)
            return self.session

    model = Model()
    dispatch = AsyncMock(
        side_effect=[f"Fictional result {index}" for index in range(len(names) - 1)]
    )
    monkeypatch.setattr(tools, "execute", dispatch)
    instance = run(bundle, model=model)
    outcome = await instance.run()

    assert model.request is instance._request
    assert [item.args[0].name for item in dispatch.await_args_list] == names[:-1]
    assert len(outcome.executed) == len(names) - 1
    expected = codec.encode_items(instance._request.prompt)
    for index, input_items in enumerate(executor.inputs):
        assert input_items == expected
        if index < len(names) - 1:
            expected = [
                *expected,
                *executor.outputs[index],
                *codec.encode_items(
                    (ToolResult(turns[index].tool_calls[0].call_id, f"Fictional result {index}"),)
                ),
            ]
    assert model.session._input == []


async def test_identical_paid_attempts_still_recheck_reply_budget(bundle, monkeypatch):
    name = "recall_events"
    arguments = '{"question":"Fictional repeated question"}'
    session = Session(
        [
            ModelTurn(
                tool_calls=tuple(
                    turn(f"paid-{index}", name, arguments).tool_calls[0] for index in range(3)
                )
            ),
            turn("finish", tools.FINISH),
        ]
    )
    budget = fake_budget()

    async def execute(call, **kwargs):
        await budget.check()
        await budget.record(
            kind="search",
            model="fictional",
            cny=bundle.default.budget.per_reply_cny / 2,
            group_id=kwargs["group_id"],
        )
        return "Fictional paid result"

    dispatch = AsyncMock(side_effect=execute)
    monkeypatch.setattr(tools, "execute", dispatch)
    outcome = await run(bundle, session, budget=budget).run()

    assert dispatch.await_count == 2 and len(outcome.executed) == 2
    assert budget.ledger.ledger_add.await_count == 2
    results, directive = session.seen[0]
    assert [result.output for result in results] == [
        "Fictional paid result",
        "Fictional paid result",
        agent.QUOTA_NOTE,
    ]
    assert directive is not None
    assert {spec.name for spec in directive.tools} == {tools.SEND, tools.FINISH}


@pytest.mark.parametrize("bound", ["turns", "calls", "results"])
async def test_repeated_live_reads_remain_bounded_by_existing_fuel(bundle, monkeypatch, bound):
    session = Session(turn(f"read-{index}", "list_scheduled_tasks") for index in count())
    dispatch = AsyncMock(return_value="Read")
    monkeypatch.setattr(tools, "execute", dispatch)
    instance = run(bundle, session)
    if bound == "calls":
        instance._fuel.calls_left = 3
    elif bound == "results":
        instance._fuel.result_bytes_left = 9

    outcome = await instance.run()

    expected_calls = {"turns": MAX_MODEL_TURNS - 1, "calls": 3, "results": 2}[bound]
    assert dispatch.await_count == expected_calls
    assert len(outcome.executed) == expected_calls
    assert session.started == expected_calls + 1
    assert len(session.seen) == expected_calls
    assert all(results[0].output == "Read" for results, _ in session.seen)
    directive = session.seen[-1][1]
    assert directive is not None
    assert {spec.name for spec in directive.tools} == {tools.SEND, tools.FINISH}
    assert instance.phase is agent.AgentPhase.FINISHED and session.closed
