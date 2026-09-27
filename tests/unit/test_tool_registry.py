"""One registration controls public schemas, parsing, side effects and dispatch."""

import json
from unittest.mock import AsyncMock

import pytest
from pydantic import ValidationError

import _db as _test_db
from qqbot.conversation import tools
from qqbot.conversation.tool_registry import ToolEntry, ToolRegistry
from qqbot.domain.ids import GroupId
from qqbot.providers.contracts import ToolCall, ToolCallId


def test_all_published_tools_have_one_execution_registration(bundle):
    registry = tools.tool_registry(bundle.default, prompts=_test_db.test_bundle().prompts)
    assert registry.definitions == tools.tool_defs(
        bundle.default, prompts=_test_db.test_bundle().prompts
    )
    for spec in registry.definitions:
        entry = registry.get(spec.name)
        assert entry is not None
        assert (entry.handler is None) == entry.exclusive
    assert registry.exclusive(tools.SEND) and registry.exclusive(tools.FINISH)
    assert registry.parallel_safe("web_search")
    assert not registry.parallel_safe("schedule_task")
    assert not registry.parallel_safe("recall_events")
    assert not registry.parallel_safe("unknown")


@pytest.mark.parametrize(
    "arguments", ["null", "[]", "3", '{"query":3}', '{"query":"x","hidden":true}', "{}"]
)
def test_wire_schema_and_runtime_argument_shape_agree(bundle, arguments):
    entry = tools.tool_registry(bundle.default, prompts=_test_db.test_bundle().prompts).get(
        "web_search"
    )
    with pytest.raises(ValidationError):
        entry.parse(arguments)


def test_optional_schedule_fields_are_absent_rather_than_fabricated(bundle):
    entry = tools.tool_registry(bundle.default, prompts=_test_db.test_bundle().prompts).get(
        "schedule_task"
    )
    assert entry.parse('{"intent":"Fictional task","delay_seconds":60}') == {
        "intent": "Fictional task",
        "delay_seconds": 60,
    }
    with pytest.raises(ValidationError):
        entry.parse('{"intent":"Fictional task","delay_seconds":true}')


async def test_invalid_arguments_never_reach_the_registered_handler(bundle):
    entry = tools.tool_registry(bundle.default, prompts=_test_db.test_bundle().prompts).get(
        "web_search"
    )
    handler = AsyncMock(return_value="Fictional output")
    registry = ToolRegistry((ToolEntry(entry.spec, handler, parallel_safe=True),))
    context = tools.ToolCtx(
        object(),
        object(),
        registry=registry,
        identities=_test_db.identities,
        database=_test_db.pool,
        clock=_test_db.clock,
    )
    call = ToolCall(ToolCallId("fictional"), "web_search", json.dumps({"query": ["not-text"]}))
    output = await tools.execute(
        call,
        cfg=bundle.default,
        group_id=GroupId("311"),
        ctx=context,
        prompts=_test_db.test_bundle().prompts,
    )
    assert isinstance(output, tools.Failure)
    handler.assert_not_awaited()
    call = ToolCall(ToolCallId("fictional"), "web_search", json.dumps({"query": "valid"}))
    assert (
        await tools.execute(
            call,
            cfg=bundle.default,
            group_id=GroupId("311"),
            ctx=context,
            prompts=_test_db.test_bundle().prompts,
        )
        == "Fictional output"
    )
    handler.assert_awaited_once()


def test_duplicate_and_missing_handler_registrations_fail_at_construction(bundle):
    entry = tools.tool_registry(bundle.default, prompts=_test_db.test_bundle().prompts).get(
        "web_search"
    )
    with pytest.raises(ValueError, match="duplicate"):
        ToolRegistry((entry, entry))
    with pytest.raises(ValueError, match="handler"):
        ToolRegistry((ToolEntry(entry.spec, None),))
