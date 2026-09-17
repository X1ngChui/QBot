"""Provider composition, codecs, session states and backend-owned pricing."""

from __future__ import annotations

import asyncio
import os
import pathlib
import sys
from datetime import datetime
from types import SimpleNamespace
from zoneinfo import ZoneInfo

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "config"))

from qqbot.providers import base
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
from qqbot.providers.local import local_text
from qqbot.providers.openai_responses import (
    ResponsesCodec,
    ResponsesTextSession,
    _parse_wire,
    _turn,
)
from qqbot.providers.registry import build
from qqbot.providers.sherpa import SherpaAsr
from qqbot.providers.tavily import TavilySearch
from qqbot.settings import (
    AsrCfg,
    ConfigBundle,
    EmbeddingCfg,
    RestartRequired,
    Settings,
    TextCfg,
    load_bundle,
)

fails: list[str] = []


def check(name: str, condition: bool, detail: str = "") -> None:
    print(f"[{'ok ' if condition else 'FAIL'}] {name}  {detail}")
    if not condition:
        fails.append(name)


def raises(kind: type[BaseException], fn) -> BaseException | None:
    try:
        fn()
    except kind as exc:
        return exc
    return None


def _text_cfg(*, provider: str = "openai_responses") -> TextCfg:
    return TextCfg.model_validate(
        {
            "provider": provider,
            "endpoint": "https://example.invalid/v1",
            "model": "m",
            "reasoning_effort": "off",
        }
    )


def _terminal(raw: dict):
    return _parse_wire(raw, fallback_model="configured", status_hint="completed")


class _ScriptExecutor:
    codec = ResponsesCodec()

    def __init__(self, wires) -> None:
        self.wires = list(wires)
        self.inputs: list[list[dict]] = []
        self.telemetry = []

    async def complete(self, input_items, tools, *, policy, context, telemetry=None):
        del tools, policy, context
        self.telemetry.append(telemetry)
        self.inputs.append(list(input_items))
        wire = self.wires.pop(0)
        return SimpleNamespace(wire=wire, turn=_turn(wire))


def config_checks(settings: Settings) -> None:
    raw_text = settings.capabilities.text.model_dump()
    check(
        "provider selection is closed by the config schema",
        raises(
            ValueError,
            lambda: TextCfg.model_validate(raw_text | {"provider": "unknown"}),
        )
        is not None,
    )
    check(
        "irrelevant provider fields are rejected",
        raises(
            ValueError,
            lambda: TextCfg.model_validate(raw_text | {"deployment_id": "unused"}),
        )
        is not None,
    )
    legacy = raises(
        ValueError,
        lambda: TextCfg.model_validate(
            {
                "backend": "deepseek",
                "base_url": "https://example.invalid",
                "api_key_env": "KEY",
                "model": "m",
            }
        ),
    )
    check(
        "legacy provider keys fail with migration names",
        legacy is not None
        and all(
            name in str(legacy)
            for name in ("provider", "endpoint", "credential_env")
        ),
        str(legacy),
    )
    check(
        "group overrides cannot rebuild process-owned providers",
        raises(
            ValueError,
            lambda: ConfigBundle._validate_overrides(
                "42", {"capabilities": {"text": {"endpoint": "elsewhere"}}}
            ),
        )
        is not None,
    )
    check(
        "group overrides cannot silently change shared runtime policy",
        raises(
            ValueError,
            lambda: ConfigBundle._validate_overrides(
                "42", {"gateway": {"member_cache_ttl_sec": 1}}
            ),
        )
        is not None,
    )
    check(
        "group overrides cannot replace the process-owned extraction policy",
        raises(
            ValueError,
            lambda: ConfigBundle._validate_overrides(
                "42", {"capabilities": {"text": {"extract": {"model": "other"}}}}
            ),
        )
        is not None,
    )
    check(
        "group overrides can change task-local policy",
        ConfigBundle._validate_overrides(
            "42", {"gateway": {"media_wait_sec": 1}}
        )
        == {"gateway": {"media_wait_sec": 1}},
    )

    import qqbot.settings as settings_module
    from qqbot import util

    current = ConfigBundle(settings.model_dump(), {})
    changed = settings.model_copy(deep=True)
    changed.capabilities.text.endpoint = "https://restart.invalid"
    fresh = ConfigBundle(changed.model_dump(), {})
    original_bundle = settings_module._bundle
    original_loader = settings_module.load_bundle
    original_timezone = util.tz()
    settings_module._bundle = current
    settings_module.load_bundle = lambda: fresh
    try:
        rejected = raises(RestartRequired, settings_module.reload_config)
        check(
            "restart-scoped reload is atomic",
            rejected is not None and settings_module._bundle is current,
            str(rejected),
        )
        check(
            "rejected reload leaves the active timezone unchanged",
            util.tz() == original_timezone,
        )
        hot = settings.model_copy(deep=True)
        hot.gateway.max_msg_len += 1
        hot_bundle = ConfigBundle(hot.model_dump(), {})
        settings_module.load_bundle = lambda: hot_bundle
        applied = settings_module.reload_config()
        check(
            "a fully reloadable bundle swaps atomically",
            applied is hot_bundle and settings_module._bundle is hot_bundle,
        )
    finally:
        settings_module.load_bundle = original_loader
        settings_module._bundle = original_bundle


async def session_checks() -> None:
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
    turn = await session.start()
    check("a session exposes neutral calls only", turn.tool_calls[0].name == "lookup")
    try:
        await session.start()
        check("a session cannot start twice", False)
    except RuntimeError:
        check("a session cannot start twice", True)
    try:
        await session.continue_with((ToolResult(ToolCallId("wrong"), "x"),))
        check("continuation ids must match exactly", False)
    except ModelFailure as exc:
        check(
            "continuation ids must match exactly",
            exc.kind is FailureKind.PROTOCOL and exc.charge is ChargeState.NOT_SENT,
        )
    done = await session.continue_with((ToolResult(ToolCallId("call-1"), "result"),))
    check("a matching result advances the private replay", done.text == "done")
    check(
        "explicit replay stays inside the session",
        [item.get("type") for item in executor.inputs[1]]
        == [None, None, "function_call", "function_call_output"],
        str(executor.inputs[1]),
    )
    check(
        "cache telemetry identifies one replay session without carrying prompt text",
        executor.telemetry[0].phase == "initial"
        and executor.telemetry[1].phase == "continuation"
        and executor.telemetry[0].run_id == executor.telemetry[1].run_id
        and executor.telemetry[0].stable_prefix_hash
        == executor.telemetry[1].stable_prefix_hash
        and "private policy" not in repr(executor.telemetry),
        repr(executor.telemetry),
    )
    try:
        await session.continue_with(())
        check("a completed session cannot continue", False)
    except RuntimeError:
        check("a completed session cannot continue", True)
    await session.aclose()


def main() -> int:
    settings = load_bundle().default
    config_checks(settings)
    bundle = build(settings)
    expected = (
        f"text={settings.capabilities.text.provider} "
        f"vision={settings.capabilities.vision.provider} "
        f"asr=sherpa "
        f"embedding={settings.capabilities.embedding.provider} "
        f"search={settings.capabilities.search.provider}"
    )
    check(
        "composition root wires all capabilities",
        bundle.describe() == expected,
        bundle.describe(),
    )
    check("production ASR is fixed local CPU", isinstance(bundle.asr, SherpaAsr))
    check("search implements its own capability", isinstance(bundle.search, TavilySearch))
    check(
        "page reading is injected explicitly and shares the Tavily runtime",
        bundle.page_reader is bundle.search,
    )

    bad = settings.model_copy(deep=True)
    bad.capabilities.vision.provider = "nope"
    check(
        "an unknown selected provider is rejected",
        raises(RuntimeError, lambda: build(bad)) is not None,
    )

    class Incomplete(base.TextModel):
        name = "incomplete"

    check(
        "an incomplete capability cannot be constructed",
        raises(TypeError, Incomplete) is not None,
    )

    usage = {
        "input_tokens": 1000,
        "output_tokens": 66,
        "input_tokens_details": {"cached_tokens": 960},
        "output_tokens_details": {"reasoning_tokens": 58},
    }
    parsed = _terminal(
        {"model": "served", "status": "completed", "output": [], "usage": usage}
    )
    check(
        "terminal parsing preserves cache and reasoning usage",
        (
            parsed.usage.input_cached,
            parsed.usage.input_uncached,
            parsed.usage.output,
            parsed.usage.reasoning,
        )
        == (960, 40, 66, 58),
        str(parsed.usage),
    )
    conservative = _terminal(
        {"status": "completed", "output": [], "usage": {"input_tokens": 100}}
    )
    check(
        "missing cache details bill every input token as a miss",
        (conservative.usage.input_cached, conservative.usage.input_uncached) == (0, 100),
    )

    completed = _terminal(
        {
            "status": "completed",
            "output": [
                {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "private"}]},
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "visible"}],
                },
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
    check("reasoning never enters the neutral turn", turn.text == "visible")
    check(
        "function calls retain only executable fields",
        len(turn.tool_calls) == 1
        and turn.tool_calls[0].call_id == ToolCallId("call-1")
        and turn.tool_calls[0].arguments == '{"q":"x"}',
    )
    incomplete = _terminal(
        {
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [],
        }
    )
    failure = raises(ModelFailure, lambda: _turn(incomplete))
    check(
        "an incomplete terminal response cannot execute",
        isinstance(failure, ModelFailure) and failure.kind is FailureKind.INCOMPLETE,
    )
    malformed = _terminal(
        {
            "status": "completed",
            "output": [{"type": "function_call", "name": "lookup", "arguments": "{}"}],
        }
    )
    check(
        "a function call without call_id is a protocol failure",
        raises(ModelFailure, lambda: _turn(malformed)) is not None,
    )

    generic = ResponsesCodec()
    deepseek = DeepSeekResponsesCodec()
    check(
        "standard Responses requests encrypted reasoning replay",
        generic.request_extras(ReasoningEffort.OFF)
        == {"include": ["reasoning.encrypted_content"]},
    )
    check(
        "DeepSeek maps developer to user and uses its own off grade",
        deepseek.role(Role.DEVELOPER) == "user"
        and deepseek.request_extras(ReasoningEffort.OFF)
        == {"reasoning": {"effort": "none"}},
    )
    neutral = (
        Message(
            Role.USER,
            (TextPart("picture"), StoredImage("openai_responses", "file-1")),
        ),
    )
    wired = generic.encode_items(neutral)
    check(
        "the codec owns multimodal wire translation",
        wired[0]["content"]
        == [
            {"type": "input_text", "text": "picture"},
            {"type": "input_image", "file_id": "file-1"},
        ],
        str(wired),
    )
    mismatch = (
        Message(Role.USER, (StoredImage("deepseek", "file-1"),)),
    )
    check(
        "attachments cannot cross provider identity",
        raises(ModelFailure, lambda: generic.encode_items(mismatch)) is not None,
    )

    local = local_text(_text_cfg(provider="local"))
    check("the local Responses capability is keyless", not local.needs_key)
    check(
        "the local capability has explicit zero pricing",
        local.rate_for("anything").tokens(100, 100, 100) == 0.0,
    )
    request_flags = local._executor._request(
        [{"role": "user", "content": "x"}],
        [],
        GenerationPolicy("m"),
    )
    check(
        "Responses stays stateless and permits parallel tool calls",
        request_flags["store"] is False
        and request_flags["parallel_tool_calls"] is True
        and "previous_response_id" not in request_flags,
        str(request_flags),
    )
    asyncio.run(local.aclose())

    asr = AsrCfg.model_validate(
        {"model_dir": "models/asr/sense-voice", "threads": 2, "queue_capacity": 4}
    )
    check(
        "ASR config exposes resources but no provider selection",
        asr.queue_capacity == 4
        and not hasattr(asr, "provider")
        and not hasattr(asr, "credential_env")
        and not hasattr(asr, "endpoint"),
    )
    check(
        "fixed local ASR is free but explicitly metered by seconds",
        bundle.asr.rate_for("sense-voice").unit == "second"
        and bundle.asr.rate_for("sense-voice").per_unit == 0.0,
    )

    from qqbot.providers.embedding import DashScopeEmbedding

    posted: list[str] = []

    class Stop(Exception):
        pass

    class RecordingClient:
        async def post(self, url, **kwargs):
            del kwargs
            posted.append(url)
            raise Stop

    embedding = DashScopeEmbedding()
    embedding._client = lambda cfg: RecordingClient()

    def embedding_cfg(endpoint: str) -> EmbeddingCfg:
        return EmbeddingCfg.model_validate(
            {
                "provider": "dashscope",
                "endpoint": endpoint,
                "model": "m",
                "credential_env": "PATH",
            }
        )

    for url in ("https://first.example/v1", "https://second.example/v1/"):
        try:
            asyncio.run(embedding.embed(["x"], cfg=embedding_cfg(url)))
        except Stop:
            pass
    check(
        "embedding follows the endpoint in its call config",
        posted == ["https://first.example/v1/embeddings", "https://second.example/v1/embeddings"],
        str(posted),
    )

    from qqbot.providers import deepseek as prices

    beijing = ZoneInfo("Asia/Shanghai")

    def at(year, month, day, hour):
        return prices._at_peak(datetime(year, month, day, hour, 30, tzinfo=beijing))

    check("weekday pricing windows use Beijing time", at(2026, 8, 11, 10))
    check("lunch and evenings are off-peak", not at(2026, 8, 11, 12) and not at(2026, 8, 11, 21))
    check("weekends are off-peak", not at(2026, 8, 15, 10))
    off_peak = prices._rate_at(
        "deepseek-flash", datetime(2026, 9, 10, 21, 30, tzinfo=beijing)
    )
    peak = prices._rate_at(
        "deepseek-flash", datetime(2026, 9, 10, 10, 30, tzinfo=beijing)
    )
    check(
        "peak pricing doubles every token direction",
        (peak.in_hit, peak.in_miss, peak.out)
        == (off_peak.in_hit * 2, off_peak.in_miss * 2, off_peak.out * 2),
    )
    unknown = prices._rate_at(
        "no-such-model", datetime(2026, 9, 10, 21, 30, tzinfo=beijing)
    )
    check("unknown models use the pessimistic tier", unknown.out >= off_peak.out)
    check(
        "rate arithmetic stays exact",
        abs(base.Rate("Mtoken", in_hit=0.02).tokens(1_000_000, 0, 0) - 0.02) < 1e-9,
    )

    asyncio.run(session_checks())
    asyncio.run(bundle.aclose())
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(main())
