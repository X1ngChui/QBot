"""The task evaluator models live state and observations without external side effects."""

from datetime import UTC, datetime
import json
from types import SimpleNamespace

from qqbot.clock import Clock
from qqbot.conversation.agent import MessageDraft
from qqbot.conversation import tools
from qqbot.delivery.segments import AtSegment, TextSegment
from qqbot.domain.ids import AccountId, GroupId
from qqbot.providers.contracts import ToolCall, ToolCallId
from scripts.eval_tasks import CASES, EvalCase, FictionalTasks


def call(name, args):
    return ToolCall(ToolCallId("fictional"), name, json.dumps(args))


async def test_each_repeated_task_request_has_a_real_effect_and_live_list(bundle):
    clock = Clock("Asia/Shanghai", wall=lambda: datetime(2030, 1, 2, tzinfo=UTC))
    simulation = FictionalTasks(EvalCase("fictional", "Fictional request"), clock)
    ctx = SimpleNamespace(registry=tools.tool_registry(bundle.default, prompts=bundle.prompts))

    async def execute(request):
        return await simulation.execute(
            request, cfg=bundle.default, group_id=GroupId("311"), ctx=ctx
        )

    before = await execute(call("list_scheduled_tasks", {}))
    assert "没有" in before
    args = {"intent": "Fictional task", "delay_seconds": 300}
    await execute(call("schedule_task", args))
    await execute(call("schedule_task", args))
    assert len(simulation.pending) == 2
    after = await execute(call("list_scheduled_tasks", {}))
    assert all(identifier in after for identifier in simulation.pending)
    identifier = next(iter(simulation.pending))
    await execute(call("cancel_scheduled_task", {"id": identifier}))
    final = await execute(call("list_scheduled_tasks", {}))
    assert identifier not in final and len(simulation.pending) == 1
    assert len(simulation.events) == 6


async def test_short_delay_is_failure_and_does_not_create_task(bundle):
    simulation = FictionalTasks(EvalCase("fictional", "Fictional request"), Clock("Asia/Shanghai"))
    result = await simulation.execute(
        call("schedule_task", {"intent": "Fictional task", "delay_seconds": 120}),
        cfg=bundle.default,
        group_id=GroupId("311"),
        ctx=SimpleNamespace(registry=tools.tool_registry(bundle.default, prompts=bundle.prompts)),
    )
    assert isinstance(result, tools.Failure)
    assert simulation.pending == {} and simulation.creations == 0


async def test_simulated_send_retains_real_recipient_and_returns_observation():
    case = next(case for case in CASES if case.name == "one-shot-wakeup-no-loop")
    simulation = FictionalTasks(case, Clock("Asia/Shanghai"))
    draft = MessageDraft((AtSegment("202"), TextSegment("Fictional reminder")))
    result = await simulation.send(draft, ())
    assert result.confirmed and result.observed is not None
    assert simulation.drafts[0].at == [AccountId("202")]
    assert simulation.failures() == []


def test_partial_operation_and_missing_final_query_never_pass_evaluation():
    case = next(case for case in CASES if case.name == "merge-and-final-list")
    simulation = FictionalTasks(case, Clock("Asia/Shanghai"))
    simulation.messages.append("Fictional completion claim")
    failures = simulation.failures()
    assert "merge did not retain exactly one replacement plus unrelated reminder" in failures
    assert "missing fresh final verification after mutations" in failures
