"""Provider composition, codecs, replay sessions and local pricing."""

from datetime import datetime
from types import SimpleNamespace
from zoneinfo import ZoneInfo

import pytest

from _budget import fake_budget
from _fixtures import example_bundle
from qqbot.configuration import AsrCfg, ConfigBundle, EmbeddingCfg, Persona, TextCfg
from qqbot.domain.ids import GroupId
from qqbot.providers import base, deepseek as prices
from qqbot.providers.contracts import (
    CallContext,
    ChargeState,
    FailureKind,
    GenerationPolicy,
    Message,
    ModelFailure,
    ModelRequest,
    ReasoningEffort,
    Role,
    StoredImage,
    TextPart,
    ToolCallId,
    ToolResult,
)
from qqbot.providers.deepseek import DeepSeekResponsesCodec
from qqbot.providers.embedding import DashScopeEmbedding
from qqbot.providers.local import local_text
from qqbot.providers.openai_responses import (
    ResponsesCodec,
    ResponsesExecutor,
    ResponsesTextSession,
    _parse_wire,
    _turn,
)
from qqbot.providers.registry import build
from qqbot.providers.sherpa import SherpaAsr
from qqbot.providers.tavily import TavilySearch


def _text_cfg(*, provider="openai_responses"):
    return TextCfg.model_validate(
        {
            "provider": provider,
            "endpoint": "https://example.invalid/v1",
            "model": "m",
            "reasoning_effort": "off",
        }
    )


def _terminal(raw):
    return _parse_wire(raw, fallback_model="configured", status_hint="completed")


class _ScriptExecutor:
    codec = ResponsesCodec()
    _request = ResponsesExecutor._request

    def __init__(self, wires):
        self.wires = list(wires)
        self.inputs = []
        self.telemetry = []

    async def complete(self, input_items, tools, *, policy, context, telemetry=None):
        del tools, policy, context
        self.telemetry.append(telemetry)
        self.inputs.append(list(input_items))
        wire = self.wires.pop(0)
        return SimpleNamespace(wire=wire, turn=_turn(wire))


def test_provider_schema_rejects_unknown_and_legacy_fields():
    settings = example_bundle().default
    raw = settings.backends.text.model_dump()
    with pytest.raises(ValueError):
        TextCfg.model_validate(raw | {"provider": "unknown"})
    with pytest.raises(ValueError):
        TextCfg.model_validate(raw | {"deployment_id": "unused"})
    with pytest.raises(ValueError) as error:
        TextCfg.model_validate(
            {
                "backend": "deepseek",
                "base_url": "https://example.invalid",
                "api_key_env": "KEY",
                "model": "m",
            }
        )
    assert all(key in str(error.value) for key in ("backend", "base_url", "api_key_env"))


def test_group_personas_cannot_override_global_configuration():
    settings = example_bundle().default
    bundle = ConfigBundle(settings.model_dump(), {GroupId("42"): Persona(name="Different")})
    group_settings, persona = bundle.for_group(GroupId("42"))
    assert group_settings is bundle.default
    assert persona.name == "Different"
    with pytest.raises(ValueError):
        Persona.model_validate({"name": "Different", "overrides": {"media": {"wait_sec": 1}}})
    with pytest.raises(ValueError):
        settings.backends.text.model = "changed"


@pytest.mark.asyncio
async def test_registry_composes_capabilities_and_closes_them():
    settings = example_bundle().default
    bundle = build(settings, budget=fake_budget())
    try:
        assert bundle.describe() == (
            f"text={settings.backends.text.provider} vision={settings.backends.vision.provider} "
            f"asr=sherpa embedding={settings.backends.embedding.provider} "
            f"search={settings.backends.search.provider}"
        )
        assert isinstance(bundle.asr, SherpaAsr)
        assert isinstance(bundle.search, TavilySearch)
        assert bundle.page_reader is bundle.search
        assert bundle.asr.rate_for("sense-voice").unit == "second"
        assert bundle.asr.rate_for("sense-voice").per_unit == 0.0
    finally:
        await bundle.aclose()


def test_registry_rejects_unknown_provider_and_incomplete_capability():
    settings = example_bundle().default
    bad = settings.model_copy(
        update={
            "backends": settings.backends.model_copy(
                update={"vision": settings.backends.vision.model_copy(update={"provider": "nope"})}
            )
        }
    )
    with pytest.raises(RuntimeError):
        build(bad, budget=fake_budget())

    class Incomplete(base.TextModel):
        name = "incomplete"

    with pytest.raises(TypeError):
        Incomplete()


def test_usage_parses_cached_and_reasoning_tokens():
    parsed = _terminal(
        {
            "model": "served",
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 1000,
                "output_tokens": 66,
                "input_tokens_details": {"cached_tokens": 960},
                "output_tokens_details": {"reasoning_tokens": 58},
            },
        }
    )
    assert (
        parsed.usage.input_cached,
        parsed.usage.input_uncached,
        parsed.usage.output,
        parsed.usage.reasoning,
    ) == (960, 40, 66, 58)
    conservative = _terminal({"status": "completed", "output": [], "usage": {"input_tokens": 100}})
    assert (conservative.usage.input_cached, conservative.usage.input_uncached) == (0, 100)


def test_turn_strips_reasoning_and_keeps_executable_calls():
    completed = _terminal(
        {
            "status": "completed",
            "output": [
                {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "private"}]},
                {"type": "message", "content": [{"type": "output_text", "text": "visible"}]},
                {
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "lookup",
                    "arguments": '{"q":"x"}',
                },
            ],
        }
    )
    turn = _turn(completed)
    assert turn.text == "visible"
    assert len(turn.tool_calls) == 1
    assert turn.tool_calls[0].call_id == ToolCallId("call-1")
    assert turn.tool_calls[0].arguments == '{"q":"x"}'
    incomplete = _terminal(
        {
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [],
        }
    )
    with pytest.raises(ModelFailure) as failure:
        _turn(incomplete)
    assert failure.value.kind is FailureKind.INCOMPLETE
    malformed = _terminal(
        {
            "status": "completed",
            "output": [{"type": "function_call", "name": "lookup", "arguments": "{}"}],
        }
    )
    with pytest.raises(ModelFailure):
        _turn(malformed)


def test_codecs_own_provider_specific_wire_translation():
    generic = ResponsesCodec()
    deepseek = DeepSeekResponsesCodec()
    assert generic.request_extras(ReasoningEffort.OFF) == {
        "include": ["reasoning.encrypted_content"]
    }
    assert deepseek.role(Role.DEVELOPER) == "user"
    assert deepseek.request_extras(ReasoningEffort.OFF) == {"reasoning": {"effort": "none"}}
    neutral = (
        Message(Role.USER, (TextPart("picture"), StoredImage("openai_responses", "file-1"))),
    )
    assert generic.encode_items(neutral)[0]["content"] == [
        {"type": "input_text", "text": "picture"},
        {"type": "input_image", "file_id": "file-1"},
    ]
    with pytest.raises(ModelFailure):
        generic.encode_items((Message(Role.USER, (StoredImage("deepseek", "file-1"),)),))


@pytest.mark.asyncio
async def test_local_text_has_no_key_no_cost_and_stateless_requests():
    local = local_text(_text_cfg(provider="local"), base.RetryPolicy(0, 30), budget=fake_budget())
    try:
        assert not local.needs_key
        assert local.rate_for("anything").tokens(100, 100, 100) == 0.0
        flags = local._executor._request(
            [{"role": "user", "content": "x"}], [], GenerationPolicy("m")
        )
        assert flags["store"] is False
        assert flags["parallel_tool_calls"] is True
        assert "previous_response_id" not in flags
    finally:
        await local.aclose()


def test_local_asr_schema_has_only_resource_settings():
    asr = AsrCfg.model_validate({"model_dir": "models/asr/sense-voice", "threads": 2})
    assert asr.threads == 2
    assert not any(
        hasattr(asr, field)
        for field in ("queue_capacity", "provider", "credential_env", "endpoint")
    )


@pytest.mark.asyncio
async def test_embedding_retains_construction_endpoint():
    posted = []

    class Stop(Exception):
        pass

    class RecordingClient:
        async def post(self, url, **kwargs):
            del kwargs
            posted.append(url)
            raise Stop

    cfg = EmbeddingCfg.model_validate(
        {
            "provider": "dashscope",
            "endpoint": "https://first.example/v1",
            "model": "m",
            "credential_env": "PATH",
        }
    )
    embedding = DashScopeEmbedding(cfg, base.RetryPolicy(0, 30), budget=fake_budget())
    embedding._http = RecordingClient()
    for _ in range(2):
        with pytest.raises(Stop):
            await embedding.embed(["x"])
    assert posted == ["https://first.example/v1/embeddings"] * 2


def test_deepseek_peak_pricing_uses_beijing_clock():
    beijing = ZoneInfo("Asia/Shanghai")

    def at(year, month, day, hour):
        return prices._at_peak(datetime(year, month, day, hour, 30, tzinfo=beijing))

    assert at(2026, 8, 11, 10)
    assert not at(2026, 8, 11, 12)
    assert not at(2026, 8, 11, 21)
    assert not at(2026, 8, 15, 10)
    off_peak = prices._rate_at("deepseek-flash", datetime(2026, 9, 10, 21, 30, tzinfo=beijing))
    peak = prices._rate_at("deepseek-flash", datetime(2026, 9, 10, 10, 30, tzinfo=beijing))
    assert (peak.in_hit, peak.in_miss, peak.out) == (
        off_peak.in_hit * 2,
        off_peak.in_miss * 2,
        off_peak.out * 2,
    )
    assert (
        prices._rate_at("no-such-model", datetime(2026, 9, 10, 21, 30, tzinfo=beijing)).out
        >= off_peak.out
    )
    assert base.Rate("Mtoken", in_hit=0.02).tokens(1_000_000, 0, 0) == pytest.approx(0.02)


@pytest.mark.asyncio
async def test_session_replays_exact_calls_and_never_leaks_prompt_telemetry():
    first = _terminal(
        {
            "status": "completed",
            "output": [
                {
                    "type": "function_call",
                    "id": "fc-1",
                    "call_id": "call-1",
                    "name": "lookup",
                    "arguments": "{}",
                }
            ],
        }
    )
    second = _terminal(
        {
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}],
                }
            ],
        }
    )
    executor = _ScriptExecutor([first, second])
    request = ModelRequest(
        prompt=(Message(Role.SYSTEM, "private policy"), Message(Role.USER, "go")),
        tools=(),
        policy=GenerationPolicy("m"),
        context=CallContext(),
    )
    session = ResponsesTextSession(executor, request)
    try:
        turn = await session.start()
        assert turn.tool_calls[0].name == "lookup"
        with pytest.raises(RuntimeError):
            await session.start()
        with pytest.raises(ModelFailure) as failure:
            await session.continue_with((ToolResult(ToolCallId("wrong"), "x"),))
        assert failure.value.kind is FailureKind.PROTOCOL
        assert failure.value.charge is ChargeState.NOT_SENT
        done = await session.continue_with((ToolResult(ToolCallId("call-1"), "result"),))
        assert done.text == "done"
        assert [item.get("type") for item in executor.inputs[1]] == [
            None,
            None,
            "function_call",
            "function_call_output",
        ]
        assert [event.phase for event in executor.telemetry] == ["initial", "continuation"]
        assert executor.telemetry[0].run_id == executor.telemetry[1].run_id
        assert executor.telemetry[0].stable_prefix_hash == executor.telemetry[1].stable_prefix_hash
        assert "private policy" not in repr(executor.telemetry)
        with pytest.raises(RuntimeError):
            await session.continue_with(())
    finally:
        await session.aclose()
