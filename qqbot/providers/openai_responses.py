"""Provider-neutral model sessions backed by the Responses API."""

from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import logging
import time
import uuid
from collections.abc import Callable, Mapping
from dataclasses import dataclass, replace
from enum import StrEnum
from typing import Any

import openai

from ..core.budget import BUDGET
from ..settings import TextCfg, VisionCfg
from ..util import why
from .base import Rate, TextModel, TextSession, VisionModel, backoff_delay, retry_after_seconds
from .contracts import (
    AttachmentStore,
    CallContext,
    CallPurpose,
    ChargeState,
    FailureKind,
    GenerationPolicy,
    ImageBytes,
    Message,
    ModelFailure,
    ModelRequest,
    ModelTurn,
    ModelUsage,
    PromptItem,
    ReasoningEffort,
    Role,
    SessionDirective,
    StoredImage,
    TextPart,
    ToolCall,
    ToolCallId,
    ToolResult,
    ToolSpec,
)
from .openai_transport import RETRYABLE, ResponsesTransport, StreamError, StreamInterrupted

log = logging.getLogger("qqbot.responses")

UNKNOWN_TOKEN_RATE = Rate(
    "Mtoken",
    in_hit=100.0,
    in_miss=100.0,
    out=100.0,
    source="pessimistic unpriced Responses backend",
)
_RETRYABLE_ERROR_CODES = frozenset(
    {
        "internal_error",
        "internal_server_error",
        "rate_limit_exceeded",
        "server_error",
        "temporarily_unavailable",
        "timeout",
    }
)


@dataclass(frozen=True, slots=True)
class _WireResponse:
    output: tuple[dict[str, Any], ...]
    model: str
    status: str
    usage: ModelUsage
    incomplete: Mapping[str, Any] | None
    error: Mapping[str, Any] | None


@dataclass(frozen=True, slots=True)
class _CompletedTurn:
    wire: _WireResponse
    turn: ModelTurn


@dataclass(frozen=True, slots=True)
class _CacheContext:
    run_id: str
    phase: str
    stable_prefix_hash: str
    history_messages: int


class _SessionState(StrEnum):
    NEW = "new"
    WAITING = "waiting"
    DONE = "done"
    CLOSED = "closed"


def _mapping(value: Any) -> Mapping[str, Any] | None:
    if value is None:
        return None
    if isinstance(value, Mapping):
        return value
    dump = getattr(value, "model_dump", None)
    if callable(dump):
        data = dump(exclude_none=True, by_alias=True, mode="json")
        return data if isinstance(data, Mapping) else None
    return {"message": str(value)}


def _estimate_tokens(input_items: list[dict], tools: list[dict]) -> int:
    """A conservative text estimate that never counts base64 bytes as tokens."""

    def size(value: Any) -> int:
        if isinstance(value, Mapping):
            if value.get("type") == "input_image":
                return 2500
            return sum(len(str(key)) + size(part) for key, part in value.items())
        if isinstance(value, list | tuple):
            return sum(size(part) for part in value)
        return len(str(value))

    return max(1, size(input_items) + size(tools))


def _wire_part(part: TextPart | ImageBytes | StoredImage, *, provider: str) -> dict[str, Any]:
    match part:
        case TextPart(text=text):
            return {"type": "input_text", "text": text}
        case ImageBytes(data=data, media_type=media_type):
            encoded = base64.b64encode(data).decode()
            return {"type": "input_image", "image_url": f"data:{media_type};base64,{encoded}"}
        case StoredImage(provider=owner, handle=handle):
            if owner != provider:
                raise ModelFailure(
                    f"attachment belongs to {owner!r}, not {provider!r}",
                    kind=FailureKind.PROTOCOL,
                    charge=ChargeState.NOT_SENT,
                )
            return {"type": "input_image", "file_id": handle}
    raise AssertionError(f"unhandled input part: {part!r}")


def _wire_items(
    items: tuple[PromptItem, ...], *, provider: str, role: Callable[[Role], str]
) -> list[dict[str, Any]]:
    wire: list[dict[str, Any]] = []
    for item in items:
        match item:
            case Message(role=item_role, content=str() as content):
                wire.append({"role": role(item_role), "content": content})
            case Message(role=item_role, content=parts):
                wire.append(
                    {
                        "role": role(item_role),
                        "content": [_wire_part(part, provider=provider) for part in parts],
                    }
                )
            case ToolCall(call_id=call_id, name=name, arguments=arguments):
                wire.append(
                    {
                        "type": "function_call",
                        "call_id": str(call_id),
                        "name": name,
                        "arguments": arguments,
                    }
                )
            case ToolResult(call_id=call_id, output=str() as output):
                wire.append(
                    {"type": "function_call_output", "call_id": str(call_id), "output": output}
                )
            case ToolResult(call_id=call_id, output=parts):
                wire.append(
                    {
                        "type": "function_call_output",
                        "call_id": str(call_id),
                        "output": [_wire_part(part, provider=provider) for part in parts],
                    }
                )
            case _:
                raise AssertionError(f"unhandled prompt item: {item!r}")
    return wire


def _wire_tools(tools: tuple[ToolSpec, ...]) -> list[dict[str, Any]]:
    return [
        {
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": dict(tool.parameters),
            "strict": tool.strict,
        }
        for tool in tools
    ]


class ResponsesCodec:
    """One provider's Responses dialect; model sessions compose this object."""

    name = "openai_responses"

    def role(self, role: Role) -> str:
        return role.value

    def encode_items(self, items: tuple[PromptItem, ...]) -> list[dict[str, Any]]:
        return _wire_items(items, provider=self.name, role=self.role)

    def encode_tools(self, tools: tuple[ToolSpec, ...]) -> list[dict[str, Any]]:
        return _wire_tools(tools)

    def request_extras(self, effort: ReasoningEffort) -> dict[str, Any]:
        extras: dict[str, Any] = {"include": ["reasoning.encrypted_content"]}
        if effort is not ReasoningEffort.OFF:
            extras["reasoning"] = {
                "effort": {
                    ReasoningEffort.LOW: "low",
                    ReasoningEffort.HIGH: "high",
                    ReasoningEffort.MAX: "xhigh",
                }[effort]
            }
        return extras


def _parse_wire(raw: Mapping[str, Any], *, fallback_model: str, status_hint: str) -> _WireResponse:
    raw_usage = raw.get("usage") or {}
    if not isinstance(raw_usage, Mapping):
        raw_usage = {}
    input_details = raw_usage.get("input_tokens_details") or {}
    output_details = raw_usage.get("output_tokens_details") or {}
    if not isinstance(input_details, Mapping):
        input_details = {}
    if not isinstance(output_details, Mapping):
        output_details = {}
    total_input = int(raw_usage.get("input_tokens") or 0)
    cached = int(input_details.get("cached_tokens") or 0)
    output = raw.get("output") or []
    if not isinstance(output, list) or not all(isinstance(item, dict) for item in output):
        raise ModelFailure(
            "Responses output is not an object sequence",
            kind=FailureKind.PROTOCOL,
            charge=ChargeState.MAY_HAVE_CHARGED,
        )
    return _WireResponse(
        output=tuple(output),
        model=str(raw.get("model") or fallback_model),
        status=str(raw.get("status") or status_hint or "completed"),
        usage=ModelUsage(
            input_cached=cached,
            input_uncached=max(0, total_input - cached),
            output=int(raw_usage.get("output_tokens") or 0),
            reasoning=int(output_details.get("reasoning_tokens") or 0),
        ),
        incomplete=_mapping(raw.get("incomplete_details")),
        error=_mapping(raw.get("error")),
    )


def _failure(wire: _WireResponse) -> ModelFailure | None:
    if wire.status == "completed":
        return None
    detail = wire.error or wire.incomplete or {}
    code = str(detail.get("code") or detail.get("type") or "")
    return ModelFailure(
        f"Responses request {wire.status}: {detail}",
        kind=FailureKind.REJECTED if wire.status == "failed" else FailureKind.INCOMPLETE,
        charge=ChargeState.CHARGED,
        retryable=code in _RETRYABLE_ERROR_CODES,
    )


def _turn(wire: _WireResponse) -> ModelTurn:
    if problem := _failure(wire):
        raise problem
    text: list[str] = []
    calls: list[ToolCall] = []
    seen_calls: set[str] = set()
    seen_functions: set[str] = set()
    for item in wire.output:
        status = item.get("status")
        if status is not None and status != "completed":
            raise ModelFailure(
                f"Responses output item is not completed: {status!r}",
                kind=FailureKind.PROTOCOL,
                charge=ChargeState.CHARGED,
            )
        match item.get("type"):
            case "message":
                content = item.get("content")
                if isinstance(content, str):
                    text.append(content)
                elif isinstance(content, list):
                    text.extend(
                        str(part["text"])
                        for part in content
                        if isinstance(part, Mapping)
                        and part.get("type") == "output_text"
                        and isinstance(part.get("text"), str)
                    )
            case "function_call":
                call_id = item.get("call_id")
                function_id = item.get("id")
                name = item.get("name")
                arguments = item.get("arguments")
                if not isinstance(call_id, str) or not call_id or call_id in seen_calls:
                    raise ModelFailure(
                        "Responses function call has a missing or repeated call_id",
                        kind=FailureKind.PROTOCOL,
                        charge=ChargeState.CHARGED,
                    )
                if function_id is not None:
                    if (
                        not isinstance(function_id, str)
                        or not function_id
                        or function_id in seen_functions
                    ):
                        raise ModelFailure(
                            "Responses function call has an invalid or repeated id",
                            kind=FailureKind.PROTOCOL,
                            charge=ChargeState.CHARGED,
                        )
                    seen_functions.add(function_id)
                if not isinstance(name, str) or not name or not isinstance(arguments, str):
                    raise ModelFailure(
                        "Responses function call is malformed",
                        kind=FailureKind.PROTOCOL,
                        charge=ChargeState.CHARGED,
                    )
                seen_calls.add(call_id)
                calls.append(ToolCall(ToolCallId(call_id), name, arguments))
    return ModelTurn(
        text="".join(text).strip(),
        tool_calls=tuple(calls),
        model=wire.model,
        usage=wire.usage,
    )


class ResponsesExecutor:
    """One configured Responses account: transport, metering and concurrency."""

    def __init__(
        self,
        *,
        provider: str,
        endpoint: str,
        credential_env: str,
        max_concurrency: int,
        codec: ResponsesCodec,
        rate_for: Callable[[str], Rate],
        key_required: bool = True,
    ) -> None:
        self.provider = provider
        self.endpoint = endpoint
        self.credential_env = credential_env
        self.codec = codec
        self.rate_for = rate_for
        self.key_required = key_required
        self._transport = ResponsesTransport()
        self._gate = asyncio.Semaphore(max_concurrency)

    async def aclose(self) -> None:
        await self._transport.aclose()

    def _log_cache(
        self,
        telemetry: _CacheContext | None,
        *,
        policy: GenerationPolicy,
        context: CallContext,
        status: str,
        started_at: float,
        usage: ModelUsage | None = None,
    ) -> None:
        """Emit cache measurements without prompt content or response identifiers."""

        if telemetry is None:
            return
        usage = usage or ModelUsage()
        record = {
            "run_id": telemetry.run_id,
            "provider": self.provider,
            "model": policy.model,
            "purpose": context.purpose.value,
            "phase": telemetry.phase,
            "strategy": "replay",
            "stable_prefix_hash": telemetry.stable_prefix_hash,
            "history_messages": telemetry.history_messages,
            "input_cached": usage.input_cached,
            "input_uncached": usage.input_uncached,
            "output": usage.output,
            "reasoning": usage.reasoning,
            "estimated": usage.estimated,
            "cny": usage.cny,
            "latency_ms": round((time.perf_counter() - started_at) * 1000),
            "status": status,
        }
        log.info("responses_cache %s", json.dumps(record, sort_keys=True))

    async def _book(
        self,
        usage: ModelUsage,
        *,
        model: str,
        context: CallContext,
    ) -> ModelUsage:
        cny = await BUDGET.record(
            kind=context.purpose,
            model=model,
            cny=self.rate_for(model).tokens(
                usage.input_cached, usage.input_uncached, usage.output
            ),
            group_id=context.group_id,
            in_hit=usage.input_cached,
            in_miss=usage.input_uncached,
            out=usage.output,
        )
        return replace(usage, cny=cny)

    async def _book_estimate(
        self,
        input_items: list[dict],
        tools: list[dict],
        *,
        policy: GenerationPolicy,
        context: CallContext,
    ) -> ModelUsage:
        usage = ModelUsage(
            input_uncached=_estimate_tokens(input_items, tools),
            output=policy.max_output_tokens or 4096,
            estimated=True,
        )
        return await self._book(usage, model=policy.model, context=context)

    def _request(
        self,
        input_items: list[dict],
        tools: list[dict],
        policy: GenerationPolicy,
    ) -> dict[str, Any]:
        request: dict[str, Any] = {
            "model": policy.model,
            "input": input_items,
            "stream": True,
            "store": False,
            "parallel_tool_calls": True,
            "timeout": policy.timeout_sec,
            **self.codec.request_extras(policy.reasoning),
        }
        if tools:
            request["tools"] = tools
        if policy.max_output_tokens is not None:
            request["max_output_tokens"] = policy.max_output_tokens
        return request

    async def complete(
        self,
        input_items: list[dict],
        tools: list[dict],
        *,
        policy: GenerationPolicy,
        context: CallContext,
        telemetry: _CacheContext | None = None,
    ) -> _CompletedTurn:
        started_at = time.perf_counter()
        attempt = 0
        while True:
            booked = False
            retry_error: BaseException | None = None
            retry_after: float | None = None
            try:
                async with self._gate:
                    terminal = await asyncio.wait_for(
                        self._transport.complete(
                            self._request(input_items, tools, policy),
                            endpoint=self.endpoint,
                            credential_env=self.credential_env,
                            timeout=policy.timeout_sec,
                            purpose=context.purpose.value,
                            key_required=self.key_required,
                        ),
                        timeout=policy.timeout_sec,
                    )
                wire = _parse_wire(
                    terminal.data,
                    fallback_model=policy.model,
                    status_hint=terminal.status_hint,
                )
                usage = wire.usage
                if not (usage.input_cached + usage.input_uncached + usage.output):
                    usage = await self._book_estimate(
                        input_items, tools, policy=policy, context=context
                    )
                else:
                    usage = await self._book(usage, model=wire.model, context=context)
                booked = True
                wire = replace(wire, usage=usage)
                turn = _turn(wire)
                self._log_cache(
                    telemetry,
                    policy=policy,
                    context=context,
                    status="ok",
                    started_at=started_at,
                    usage=usage,
                )
                return _CompletedTurn(wire, turn)
            except ModelFailure as exc:
                if booked and exc.retryable:
                    retry_error = exc
                else:
                    self._log_cache(
                        telemetry,
                        policy=policy,
                        context=context,
                        status=exc.kind.value,
                        started_at=started_at,
                    )
                    raise
            except StreamError as exc:
                if exc.started:
                    await self._book_estimate(input_items, tools, policy=policy, context=context)
                data = exc.data if isinstance(exc.data, Mapping) else {}
                code = str(data.get("code") or data.get("type") or "")
                failure = ModelFailure(
                    str(exc),
                    kind=FailureKind.UNAVAILABLE,
                    charge=(ChargeState.MAY_HAVE_CHARGED if exc.started else ChargeState.NOT_SENT),
                    retryable=code in _RETRYABLE_ERROR_CODES,
                )
                if not failure.retryable:
                    self._log_cache(
                        telemetry,
                        policy=policy,
                        context=context,
                        status=failure.kind.value,
                        started_at=started_at,
                    )
                    raise failure from exc
                retry_error = failure
            except StreamInterrupted as exc:
                if exc.started:
                    await self._book_estimate(input_items, tools, policy=policy, context=context)
                retry_error = ModelFailure(
                    str(exc),
                    kind=FailureKind.UNAVAILABLE,
                    charge=(ChargeState.MAY_HAVE_CHARGED if exc.started else ChargeState.NOT_SENT),
                    retryable=True,
                )
            except RETRYABLE as exc:
                may_charge = isinstance(exc, (TimeoutError, openai.APITimeoutError))
                if may_charge:
                    await self._book_estimate(input_items, tools, policy=policy, context=context)
                if isinstance(exc, openai.RateLimitError):
                    retry_after = retry_after_seconds(exc.response.headers)
                retry_error = ModelFailure(
                    str(exc),
                    kind=FailureKind.UNAVAILABLE,
                    charge=ChargeState.MAY_HAVE_CHARGED if may_charge else ChargeState.NOT_CHARGED,
                    retryable=True,
                )
            attempt += 1
            if attempt > policy.retries:
                assert retry_error is not None
                self._log_cache(
                    telemetry,
                    policy=policy,
                    context=context,
                    status=retry_error.kind.value,
                    started_at=started_at,
                )
                log.warning("Responses model %s out of retries: %s", policy.model, why(retry_error))
                raise retry_error
            await asyncio.sleep(backoff_delay(attempt, retry_after=retry_after))


class ResponsesTextSession(TextSession):
    """Task-local explicit replay; no native response state escapes this object."""

    def __init__(self, executor: ResponsesExecutor, request: ModelRequest) -> None:
        self._executor = executor
        stable: list[PromptItem] = []
        for item in request.prompt:
            if isinstance(item, Message) and item.role in (Role.SYSTEM, Role.DEVELOPER):
                stable.append(item)
            else:
                break
        encoded_stable = executor.codec.encode_items(tuple(stable))
        stable_bytes = json.dumps(
            encoded_stable,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode()
        self._run_id = uuid.uuid4().hex
        self._stable_prefix_hash = hashlib.sha256(stable_bytes).hexdigest()[:16]
        user_messages = sum(
            isinstance(item, Message) and item.role is Role.USER
            for item in request.prompt
        )
        self._history_messages = max(0, user_messages - 1)
        self._step_index = 0
        self._input = executor.codec.encode_items(request.prompt)
        self._tools = executor.codec.encode_tools(request.tools)
        self._policy = request.policy
        self._context = request.context
        self._state = _SessionState.NEW
        self._last_output: tuple[dict[str, Any], ...] = ()
        self._expected: tuple[ToolCallId, ...] = ()

    async def _step(self) -> ModelTurn:
        telemetry = _CacheContext(
            run_id=self._run_id,
            phase="initial" if self._step_index == 0 else "continuation",
            stable_prefix_hash=self._stable_prefix_hash,
            history_messages=self._history_messages,
        )
        completed = await self._executor.complete(
            self._input,
            self._tools,
            policy=self._policy,
            context=self._context,
            telemetry=telemetry,
        )
        self._step_index += 1
        self._last_output = completed.wire.output
        self._expected = tuple(call.call_id for call in completed.turn.tool_calls)
        self._state = _SessionState.WAITING if self._expected else _SessionState.DONE
        return completed.turn

    async def start(self) -> ModelTurn:
        if self._state is not _SessionState.NEW:
            raise RuntimeError(f"session cannot start from {self._state}")
        return await self._step()

    async def continue_with(
        self,
        results: tuple[ToolResult, ...],
        *,
        directive: SessionDirective | None = None,
    ) -> ModelTurn:
        if self._state is not _SessionState.WAITING:
            raise RuntimeError(f"session cannot continue from {self._state}")
        got = tuple(result.call_id for result in results)
        if got != self._expected:
            raise ModelFailure(
                f"tool result ids {got!r} do not match calls {self._expected!r}",
                kind=FailureKind.PROTOCOL,
                charge=ChargeState.NOT_SENT,
            )
        self._input.extend(self._last_output)
        self._input.extend(self._executor.codec.encode_items(results))
        if directive is not None:
            self._input.extend(self._executor.codec.encode_items(directive.prompt))
            if directive.tools is not None:
                self._tools = self._executor.codec.encode_tools(directive.tools)
            if directive.policy is not None:
                self._policy = directive.policy
        return await self._step()

    async def aclose(self) -> None:
        self._state = _SessionState.CLOSED
        self._input.clear()
        self._tools.clear()
        self._last_output = ()
        self._expected = ()


class ResponsesTextModel(TextModel):
    def __init__(
        self,
        cfg: TextCfg,
        *,
        name: str,
        codec: ResponsesCodec,
        rate_for: Callable[[str], Rate],
        key_required: bool = True,
        attachments: AttachmentStore | None = None,
    ) -> None:
        self.name = name
        self.needs_key = key_required
        self.attachments = attachments
        self._rate_for = rate_for
        self._executor = ResponsesExecutor(
            provider=name,
            endpoint=cfg.endpoint,
            credential_env=cfg.credential_env,
            max_concurrency=cfg.max_concurrency,
            codec=codec,
            rate_for=rate_for,
            key_required=key_required,
        )

    def open_session(self, request: ModelRequest) -> TextSession:
        return ResponsesTextSession(self._executor, request)

    def rate_for(self, model: str) -> Rate:
        return self._rate_for(model)

    async def aclose(self) -> None:
        await self._executor.aclose()
        if self.attachments is not None:
            await self.attachments.aclose()


class ResponsesVisionModel(VisionModel):
    def __init__(
        self,
        cfg: VisionCfg,
        *,
        name: str,
        codec: ResponsesCodec,
        rate_for: Callable[[str], Rate],
        key_required: bool = True,
    ) -> None:
        self.name = name
        self.needs_key = key_required
        self._rate_for = rate_for
        self._executor = ResponsesExecutor(
            provider=name,
            endpoint=cfg.endpoint,
            credential_env=cfg.credential_env,
            max_concurrency=1,
            codec=codec,
            rate_for=rate_for,
            key_required=key_required,
        )

    def rate_for(self, model: str) -> Rate:
        return self._rate_for(model)

    async def describe(
        self,
        data: bytes,
        *,
        cfg: VisionCfg,
        prompt: str,
        mime: str = "image/jpeg",
        group_id: str | None = None,
    ) -> str:
        policy = GenerationPolicy(
            model=cfg.model,
            reasoning=ReasoningEffort(cfg.reasoning_effort),
            timeout_sec=cfg.timeout_sec,
            retries=0,
            # Reasoning tokens share this ceiling with the short visible description.
            # Low-effort vision can still consume more than 1k before emitting text.
            max_output_tokens=4096,
        )
        items = (
            Message(
                Role.USER,
                (ImageBytes(data, mime), TextPart(prompt.strip())),
            ),
        )
        completed = await self._executor.complete(
            self._executor.codec.encode_items(items),
            [],
            policy=policy,
            context=CallContext(purpose=CallPurpose.VISION, group_id=group_id),
        )
        return " ".join(completed.turn.text.split())

    async def aclose(self) -> None:
        await self._executor.aclose()


class OpenAIResponses(ResponsesTextModel):
    """Generic OpenAI-style Responses text backend."""

    def __init__(self, cfg: TextCfg) -> None:
        super().__init__(
            cfg,
            name="openai_responses",
            codec=ResponsesCodec(),
            rate_for=lambda _model: UNKNOWN_TOKEN_RATE,
        )


def openai_vision(cfg: VisionCfg) -> ResponsesVisionModel:
    return ResponsesVisionModel(
        cfg,
        name="openai_responses",
        codec=ResponsesCodec(),
        rate_for=lambda _model: UNKNOWN_TOKEN_RATE,
    )
