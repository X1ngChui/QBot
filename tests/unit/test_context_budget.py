"""Offline resource contract tests with synthetic provider data only."""

from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from qqbot.configuration.schema import MediaCfg
from qqbot.conversation import prompt as reply_prompt
from qqbot.providers.payload import (
    MAX_TEXT_BYTES,
    MAX_CONTEXT_NODES,
    check_projection,
    measure,
)
from qqbot.providers.contracts import (
    CallContext,
    ChargeState,
    ContextBudgetExceeded,
    GenerationPolicy,
    ImageBytes,
    Message,
    ModelRequest,
    ModelTurn,
    Role,
    SessionDirective,
    ToolCall,
    ToolCallId,
    ToolResult,
    ToolSpec,
)
from qqbot.providers.openai_responses import (
    ResponsesCodec,
    ResponsesExecutor,
    ResponsesTextSession,
    ResponsesVisionModel,
    _CompletedTurn,
    _WireResponse,
)
from qqbot.providers.contracts import ModelUsage
from qqbot.providers.base import Rate, RetryPolicy
from qqbot.providers.openai_transport import ResponsesTransport, TerminalResponse


def _executor():
    executor = SimpleNamespace(codec=ResponsesCodec(), complete=AsyncMock())
    executor._request = lambda items, tools, policy: ResponsesExecutor._request(
        executor, items, tools, policy
    )
    return executor


def _request(*, text="hello", tools=()):
    return ModelRequest((Message(Role.SYSTEM, text),), tools, GenerationPolicy("fictional"))


def test_roster_is_all_or_rejected_before_codec_encoding():
    executor = _executor()
    executor.codec.encode_items = lambda _: pytest.fail("oversized roster was encoded")
    with pytest.raises(ContextBudgetExceeded) as rejected:
        ResponsesTextSession(executor, _request(text="x" * MAX_TEXT_BYTES))
    assert rejected.value.charge is ChargeState.NOT_SENT


def test_accepted_roster_preserves_every_member_and_number():
    members = [f"member-{number} ⟦{number}⟧" for number in range(1, 2001)]
    session = ResponsesTextSession(_executor(), _request(text="\n".join(members)))
    assert session._input[0]["content"].splitlines() == members


def test_default_vision_payload_fits_expanded_budget():
    image = ImageBytes(b"x" * (8 * 1024 * 1024), "image/jpeg")
    assert measure(Message(Role.USER, (image,))).image_bytes == len(image.data)


def test_roster_render_rejects_before_joining_large_member_data():
    catalog = SimpleNamespace(render=lambda *args, **kwargs: pytest.fail("roster was rendered"))
    profile = {"user_id": "1", "nickname": "x" * MAX_TEXT_BYTES}
    with pytest.raises(ContextBudgetExceeded):
        reply_prompt.build_developer(
            SimpleNamespace(system_prompt="", group_knowledge=""),
            [profile],
            prompts=catalog,
        )


def test_tool_schema_and_structure_share_request_budget():
    schema = {str(index): index for index in range(MAX_CONTEXT_NODES)}
    with pytest.raises(ContextBudgetExceeded):
        ResponsesTextSession(
            _executor(),
            _request(tools=(ToolSpec("lookup", "synthetic", schema),)),
        )


@pytest.mark.parametrize("length", [0, 1, 31, 256, 65_536])
def test_structural_charge_is_monotonic_for_appended_history(length):
    history = [Message(Role.USER, "é" * length)]
    assert measure((*history, Message(Role.USER, "next"))).text_bytes > measure(history).text_bytes


def test_large_attachment_and_encoded_wire_are_not_text_overflows():
    data = b"x" * (20 * 1024 * 1024)
    neutral = Message(Role.USER, (ImageBytes(data, "image/png"),))
    assert measure(neutral).image_bytes == len(data)
    wire = ResponsesCodec().encode_items((neutral,))
    assert check_projection(wire, ()).image_bytes >= len(data) - 2


def test_repeated_references_are_not_mistaken_for_cycles():
    shared = {"name": "synthetic"}
    assert measure((shared, shared)).text_bytes > 0
    cyclic = []
    cyclic.append(cyclic)
    with pytest.raises(ContextBudgetExceeded):
        measure(cyclic)


def test_output_and_replay_count_together_before_retention():
    executor = _executor()
    session = ResponsesTextSession(executor, _request(text="x" * (MAX_TEXT_BYTES // 2)))
    response = _WireResponse(
        output=({"type": "reasoning", "encrypted_content": "z" * (MAX_TEXT_BYTES // 2)},),
        model="fictional",
        status="completed",
        usage=ModelUsage(),
        incomplete=None,
        error=None,
    )
    executor.complete.return_value = _CompletedTurn(
        response, ModelTurn(tool_calls=(ToolCall(ToolCallId("call"), "lookup", "{}"),))
    )
    with pytest.raises(ContextBudgetExceeded) as rejected:
        # In the real executor the combined wire guard fires before returning.
        check_projection(
            session._input,
            session._tools,
            {"output": response.output},
            charge=ChargeState.MAY_HAVE_CHARGED,
        )
    assert rejected.value.charge is ChargeState.MAY_HAVE_CHARGED


@pytest.mark.asyncio
async def test_vision_accepts_image_above_previous_16_mib_ceiling():
    vision = object.__new__(ResponsesVisionModel)
    vision._cfg = SimpleNamespace(
        model="fictional", reasoning_effort="off", timeout_sec=1, max_output_tokens=128
    )
    vision._executor = _executor()
    wire = _WireResponse((), "fictional", "completed", ModelUsage(), None, None)
    vision._executor.complete.return_value = _CompletedTurn(wire, ModelTurn(text="synthetic"))
    image_size = 20 * 1024 * 1024
    assert image_size < MediaCfg(max_image_mb=24).max_image_mb * 1024 * 1024
    assert await vision.describe(b"x" * image_size, prompt="synthetic") == "synthetic"
    vision._executor.complete.assert_awaited_once()


@pytest.mark.asyncio
async def test_continuation_rejects_oversized_results_without_mutating_replay():
    executor = _executor()
    session = ResponsesTextSession(executor, _request())
    wire = _WireResponse((), "fictional", "completed", ModelUsage(), None, None)
    executor.complete.return_value = _CompletedTurn(
        wire, ModelTurn(tool_calls=(ToolCall(ToolCallId("call"), "lookup", "{}"),))
    )
    await session.start()
    before = list(session._input)
    with pytest.raises(ContextBudgetExceeded) as rejected:
        await session.continue_with((ToolResult(ToolCallId("call"), "z" * MAX_TEXT_BYTES),))
    assert rejected.value.charge is ChargeState.NOT_SENT
    assert session._input == before
    assert executor.complete.await_count == 1


@pytest.mark.asyncio
async def test_near_limit_request_envelope_rejects_without_mutating_continuation():
    executor = _executor()
    session = ResponsesTextSession(executor, _request())
    wire = _WireResponse((), "fictional", "completed", ModelUsage(), None, None)
    executor.complete.return_value = _CompletedTurn(
        wire, ModelTurn(tool_calls=(ToolCall(ToolCallId("call"), "lookup", "{}"),))
    )
    await session.start()
    policy = GenerationPolicy("fictional-" + "p" * 1024)
    directive = SessionDirective(policy=policy)

    def projections(result):
        results = (result,)
        neutral = check_projection(
            session._input, session._tools, (session._last_output, results, ())
        )
        next_input = [*session._input, *session._last_output, *executor.codec.encode_items(results)]
        wire_size = check_projection(next_input, session._tools)
        return neutral.text_bytes, wire_size.text_bytes, next_input

    blank = ToolResult(ToolCallId("call"), "")
    neutral_bytes, wire_bytes, _ = projections(blank)
    padding = MAX_TEXT_BYTES - max(neutral_bytes, wire_bytes) - 16
    result = ToolResult(ToolCallId("call"), "z" * padding)
    neutral_bytes, wire_bytes, next_input = projections(result)
    assert max(neutral_bytes, wire_bytes) <= MAX_TEXT_BYTES
    with pytest.raises(ContextBudgetExceeded):
        check_projection(executor._request(next_input, session._tools, policy), ())

    before = (list(session._input), list(session._tools), session._policy, session._step_index)
    with pytest.raises(ContextBudgetExceeded) as rejected:
        await session.continue_with((result,), directive=directive)
    assert rejected.value.charge is ChargeState.NOT_SENT
    assert (session._input, session._tools, session._policy, session._step_index) == before
    assert session._expected == (ToolCallId("call"),)
    executor.complete.assert_awaited_once()


@pytest.mark.asyncio
async def test_executor_rejects_combined_replay_and_vendor_reasoning_before_parsing():
    budget = SimpleNamespace(check=AsyncMock(), record=AsyncMock(return_value=0.04))
    executor = ResponsesExecutor(
        provider="synthetic",
        endpoint="https://example.invalid",
        credential_env="UNUSED",
        max_concurrency=1,
        retry=RetryPolicy(0, 0),
        codec=ResponsesCodec(),
        rate_for=lambda _: Rate("Mtoken", in_miss=1),
        budget=budget,
    )
    raw = {
        "status": "completed",
        "usage": {"input_tokens": 13, "output_tokens": 17},
        "output": [{"type": "reasoning", "encrypted_content": "r" * MAX_TEXT_BYTES}],
    }
    executor._transport.complete = AsyncMock(return_value=TerminalResponse(raw, "completed"))
    try:
        with pytest.raises(ContextBudgetExceeded) as rejected:
            await executor.complete(
                [{"role": "system", "content": "synthetic"}],
                [],
                policy=GenerationPolicy("fictional", retries=0),
                context=CallContext(),
            )
        assert rejected.value.charge is ChargeState.MAY_HAVE_CHARGED
        executor._transport.complete.assert_awaited_once()
        budget.record.assert_awaited_once()
        assert budget.record.call_args.kwargs["in_miss"] == 13
        assert budget.record.call_args.kwargs["out"] == 17
    finally:
        await executor.aclose()


@pytest.mark.asyncio
@pytest.mark.parametrize("usage", [{"input_tokens": 23, "output_tokens": 31}, None])
async def test_terminal_overflow_books_actual_or_estimated_usage_once(usage):
    budget = SimpleNamespace(check=AsyncMock(), record=AsyncMock(return_value=0.01))
    executor = ResponsesExecutor(
        provider="synthetic",
        endpoint="https://example.invalid",
        credential_env="UNUSED",
        max_concurrency=1,
        retry=RetryPolicy(2, 0),
        codec=ResponsesCodec(),
        rate_for=lambda _: Rate("Mtoken", in_miss=1),
        budget=budget,
    )
    terminal = TerminalResponse(
        {"model": "fictional", "status": "completed", "usage": usage, "output": []},
        "completed",
        overflow=True,
    )
    executor._transport.complete = AsyncMock(return_value=terminal)
    try:
        with pytest.raises(ContextBudgetExceeded) as rejected:
            await executor.complete(
                [{"role": "user", "content": "synthetic"}],
                [],
                policy=GenerationPolicy("fictional", retries=2),
                context=CallContext(),
            )
        assert rejected.value.charge is ChargeState.MAY_HAVE_CHARGED
        assert not rejected.value.retryable
        executor._transport.complete.assert_awaited_once()
        budget.record.assert_awaited_once()
        if usage is None:
            assert budget.record.call_args.kwargs["in_miss"] > 0
            assert budget.record.call_args.kwargs["out"] == 4096
        else:
            assert budget.record.call_args.kwargs["in_miss"] == 23
            assert budget.record.call_args.kwargs["out"] == 31
    finally:
        await executor.aclose()


@pytest.mark.asyncio
@pytest.mark.parametrize("usage", [{"input_tokens": 19, "output_tokens": 29}, None])
@pytest.mark.parametrize("close_fails", [False, True])
async def test_sdk_overflow_books_usage_without_serializing_output(usage, close_fails):
    budget = SimpleNamespace(check=AsyncMock(), record=AsyncMock(return_value=0.02))
    executor = ResponsesExecutor(
        provider="synthetic",
        endpoint="https://example.invalid",
        credential_env="UNUSED",
        max_concurrency=1,
        retry=RetryPolicy(2, 0),
        codec=ResponsesCodec(),
        rate_for=lambda _: Rate("Mtoken", in_miss=1),
        budget=budget,
    )
    response = SimpleNamespace(
        model="fictional",
        status="completed",
        usage=usage,
        output=[{"type": "reasoning", "encrypted_content": "z" * MAX_TEXT_BYTES}],
    )
    response.model_dump = lambda **_: pytest.fail("oversized output serialized")
    event = SimpleNamespace(type="response.completed", response=response)

    async def events():
        yield event

    class Stream:
        def __aiter__(self):
            return events()

        async def close(self):
            if close_fails:
                raise OSError("synthetic close failure")

    executor._transport._client = lambda **_: SimpleNamespace(
        responses=SimpleNamespace(create=AsyncMock(return_value=Stream()))
    )
    try:
        with pytest.raises(ContextBudgetExceeded) as rejected:
            await executor.complete(
                [{"role": "user", "content": "synthetic"}],
                [],
                policy=GenerationPolicy("fictional", retries=2),
                context=CallContext(),
            )
        assert not rejected.value.retryable
        budget.record.assert_awaited_once()
        if usage is None:
            assert budget.record.call_args.kwargs["out"] == 4096
        else:
            assert budget.record.call_args.kwargs["in_miss"] == 19
            assert budget.record.call_args.kwargs["out"] == 29
    finally:
        await executor.aclose()


@pytest.mark.asyncio
@pytest.mark.parametrize("close_fails", [False, True])
async def test_oversized_error_event_estimates_once_even_if_stream_close_fails(close_fails):
    budget = SimpleNamespace(check=AsyncMock(), record=AsyncMock(return_value=0.02))
    executor = ResponsesExecutor(
        provider="synthetic",
        endpoint="https://example.invalid",
        credential_env="UNUSED",
        max_concurrency=1,
        retry=RetryPolicy(2, 0),
        codec=ResponsesCodec(),
        rate_for=lambda _: Rate("Mtoken", in_miss=1),
        budget=budget,
    )
    event = SimpleNamespace(type="error", error={"message": "x" * MAX_TEXT_BYTES})
    event.model_dump = lambda **_: pytest.fail("oversized error serialized")

    async def events():
        yield event

    class Stream:
        def __aiter__(self):
            return events()

        async def close(self):
            if close_fails:
                raise OSError("synthetic close failure")

    create = AsyncMock(return_value=Stream())
    executor._transport._client = lambda **_: SimpleNamespace(
        responses=SimpleNamespace(create=create)
    )
    try:
        with pytest.raises(ContextBudgetExceeded) as rejected:
            await executor.complete(
                [{"role": "user", "content": "synthetic"}],
                [],
                policy=GenerationPolicy("fictional", retries=2),
                context=CallContext(),
            )
        assert rejected.value.charge is ChargeState.MAY_HAVE_CHARGED
        assert not rejected.value.retryable
        create.assert_awaited_once()
        budget.record.assert_awaited_once()
        assert budget.record.call_args.kwargs["out"] == 4096
    finally:
        await executor.aclose()


@pytest.mark.asyncio
async def test_transport_checks_reasoning_before_sdk_model_dump():
    response = SimpleNamespace(
        model="fictional",
        usage={"input_tokens": 7, "output_tokens": 11},
        output=[{"type": "reasoning", "encrypted_content": "z" * MAX_TEXT_BYTES}],
    )
    response.model_dump = lambda **_: pytest.fail("oversized response serialized")
    event = SimpleNamespace(type="response.completed", response=response)

    class Stream:
        def __aiter__(self):
            self._sent = False
            return self

        async def __anext__(self):
            if self._sent:
                raise StopAsyncIteration
            self._sent = True
            return event

        async def close(self):
            pass

    transport = ResponsesTransport()
    transport._client = lambda **_: SimpleNamespace(
        responses=SimpleNamespace(create=AsyncMock(return_value=Stream()))
    )
    terminal = await transport.complete(
        {},
        endpoint="https://example.invalid",
        credential_env="UNUSED",
        timeout=1,
        purpose="reply",
    )
    assert terminal.overflow
    assert terminal.data["usage"] == {"input_tokens": 7, "output_tokens": 11}
    assert terminal.data["model"] == "fictional"
    assert terminal.data["output"] == []


def test_roster_holder_identifiers_are_bounded_scalars(bundle):
    from uuid import UUID
    from qqbot.services.roster_cache import RosterRow

    row = RosterRow(
        user_id="fictional-account",
        entity_id=UUID("00000000-0000-0000-0000-000000000001"),
        accounts=("fictional-account",),
        nickname="Fictional member",
        former_names=(),
        aliases=(),
        memory_hints=(),
        manual_note="fictional note",
        msg_count=1,
    ).project()
    assert measure(row["entity_id"]).text_bytes == measure(str(row["entity_id"])).text_bytes
    developer = reply_prompt.build_developer(bundle._default_persona, [row], prompts=bundle.prompts)
    assert "Fictional member" in developer
    assert "fictional note" in developer
