"""OpenAI SDK transport isolated from model semantics and accounting."""

from __future__ import annotations

import asyncio
import inspect
from dataclasses import dataclass
from typing import Any

import openai
from openai import AsyncOpenAI

from ..util import read_api_key, require_key

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


class StreamError(RuntimeError):
    """A top-level Responses stream error with its provider payload."""

    def __init__(self, data: object, *, started: bool) -> None:
        super().__init__(f"Responses stream error: {data}")
        self.data = data
        self.started = started


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
        terminal: tuple[dict[str, Any], str] | None = None
        started = False
        try:
            async for event in stream:
                started = True
                event_type = getattr(event, "type", "")
                if event_type == "error":
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
                    terminal = (dump_model(response), event_type.removeprefix("response."))
        finally:
            await _close_stream(stream)
        if terminal is None:
            raise StreamInterrupted(
                "Responses stream ended before a terminal response",
                started=started,
            )
        return TerminalResponse(terminal[0], terminal[1])

    async def aclose(self) -> None:
        clients, self._clients = tuple(self._clients.values()), {}
        await asyncio.gather(*(client.close() for client in clients))
