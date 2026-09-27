"""OpenAI SDK transport isolated from model semantics and accounting."""

from __future__ import annotations

import asyncio
import inspect
from dataclasses import dataclass
from typing import Any

import openai
from openai import AsyncOpenAI

from qqbot.providers.payload import measure
from qqbot.providers.contracts import ChargeState
from qqbot.providers.contracts import ContextBudgetExceeded
from qqbot.util import read_api_key
from qqbot.util import require_key

RETRYABLE = (
    openai.APIConnectionError,
    openai.APITimeoutError,
    openai.RateLimitError,
    openai.InternalServerError,
    asyncio.TimeoutError,
)
NO_KEY_PLACEHOLDER = "local"


@dataclass(frozen=True, slots=True)
class TerminalResponse:
    data: dict[str, Any]
    status_hint: str
    overflow: bool = False


class StreamError(RuntimeError):
    """A top-level Responses stream error with its provider payload."""

    def __init__(self, data: object, *, started: bool, overflow: bool = False) -> None:
        super().__init__(f"Responses stream error: {data}")
        self.data = data
        self.started = started
        self.overflow = overflow


class StreamInterrupted(RuntimeError):
    """A stream ended without one complete terminal response."""

    def __init__(self, message: str, *, started: bool) -> None:
        super().__init__(message)
        self.started = started


def dump_model(value: Any) -> dict[str, Any]:
    if isinstance(value, dict):
        return value
    dump = getattr(value, "model_dump", None)
    if callable(dump):
        data = dump(exclude_none=True, by_alias=True, mode="json")
        if isinstance(data, dict):
            return data
    raise StreamInterrupted("terminal event carried no response object", started=True)


def _bounded_field(response: Any, name: str, fallback: Any) -> Any:
    value = (
        response.get(name, fallback)
        if isinstance(response, dict)
        else getattr(response, name, fallback)
    )
    try:
        measure(value, charge=ChargeState.MAY_HAVE_CHARGED)
    except ContextBudgetExceeded:
        return fallback
    if isinstance(value, dict | str | int | float) or value is None:
        return value
    dump = getattr(value, "model_dump", None)
    if callable(dump):
        data = dump(exclude_none=True, by_alias=True, mode="json")
        if isinstance(data, dict):
            try:
                measure(data, charge=ChargeState.MAY_HAVE_CHARGED)
            except ContextBudgetExceeded:
                return fallback
            return data
    return fallback


def _overflow_terminal(response: Any, status_hint: str) -> TerminalResponse:
    """Preserve bounded billing metadata, never serialize the oversized output."""
    return TerminalResponse(
        {
            "model": _bounded_field(response, "model", ""),
            "status": _bounded_field(response, "status", status_hint),
            "usage": _bounded_field(response, "usage", {}),
            "output": [],
        },
        status_hint,
        overflow=True,
    )


async def _close_stream(stream: Any) -> None:
    close = getattr(stream, "close", None) or getattr(stream, "aclose", None)
    if close is None:
        return
    result = close()
    if inspect.isawaitable(result):
        await result


class ResponsesTransport:
    """Connection reuse and terminal SSE collection for Responses endpoints."""

    def __init__(self) -> None:
        self._clients: dict[tuple[str, str], AsyncOpenAI] = {}

    def _client(
        self,
        *,
        endpoint: str,
        credential_env: str,
        timeout: float,
        purpose: str,
        key_required: bool,
    ) -> AsyncOpenAI:
        key = (
            require_key(credential_env, purpose)
            if key_required
            else read_api_key(credential_env) or NO_KEY_PLACEHOLDER
        )
        identity = (endpoint, key)
        client = self._clients.get(identity)
        if client is None:
            client = AsyncOpenAI(
                base_url=endpoint,
                api_key=key,
                timeout=timeout,
                max_retries=0,
            )
            self._clients[identity] = client
        return client

    async def complete(
        self,
        request: dict[str, Any],
        *,
        endpoint: str,
        credential_env: str,
        timeout: float,
        purpose: str,
        key_required: bool = True,
    ) -> TerminalResponse:
        client = self._client(
            endpoint=endpoint,
            credential_env=credential_env,
            timeout=timeout,
            purpose=purpose,
            key_required=key_required,
        )
        stream = await client.responses.create(**request)
        terminal: TerminalResponse | None = None
        overflow_error = False
        started = False
        try:
            async for event in stream:
                started = True
                event_type = getattr(event, "type", "")
                if event_type == "error":
                    try:
                        measure(event, charge=ChargeState.MAY_HAVE_CHARGED)
                    except ContextBudgetExceeded as exc:
                        overflow_error = True
                        raise StreamError(
                            {"code": "payload_overflow"}, started=True, overflow=True
                        ) from exc
                    data = dump_model(event)
                    raise StreamError(data.get("error") or data, started=True)
                if event_type in {
                    "response.completed",
                    "response.failed",
                    "response.incomplete",
                }:
                    if terminal is not None:
                        raise StreamInterrupted(
                            "Responses stream carried more than one terminal event",
                            started=True,
                        )
                    response = getattr(event, "response", None)
                    if response is None:
                        raise StreamInterrupted(
                            f"Responses terminal event {event_type} lacks response",
                            started=True,
                        )
                    status_hint = event_type.removeprefix("response.")
                    try:
                        measure(response, charge=ChargeState.MAY_HAVE_CHARGED)
                    except ContextBudgetExceeded:
                        terminal = _overflow_terminal(response, status_hint)
                        break
                    terminal = TerminalResponse(dump_model(response), status_hint)
        finally:
            try:
                await _close_stream(stream)
            except Exception:
                if not overflow_error and (terminal is None or not terminal.overflow):
                    raise
                # A failed close must not discard an overflow's billing signal.
        if terminal is None:
            raise StreamInterrupted(
                "Responses stream ended before a terminal response",
                started=started,
            )
        return terminal

    async def aclose(self) -> None:
        clients, self._clients = tuple(self._clients.values()), {}
        await asyncio.gather(*(client.close() for client in clients))
