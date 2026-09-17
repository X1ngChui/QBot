"""Shared test doubles for provider-neutral model capabilities."""

from __future__ import annotations

import hashlib
import json
from types import SimpleNamespace

from qqbot.providers.base import EmbeddingModel, Rate, TextSession
from qqbot.providers.contracts import (
    Message,
    ModelTurn,
    PromptItem,
    SessionDirective,
    TextPart,
    ToolCall,
    ToolCallId,
    ToolResult,
    ToolSpec,
)


def function_call(
    name: str,
    arguments: dict | str,
    *,
    call_id: str = "call-1",
    status: str | None = "completed",
) -> ToolCall:
    """Build one completed neutral function call for a fake model."""

    del status
    raw = arguments if isinstance(arguments, str) else json.dumps(arguments, ensure_ascii=False)
    return ToolCall(ToolCallId(call_id), name, raw)


def response(
    *,
    text: str = "",
    tool_calls: list[ToolCall] | tuple[ToolCall, ...] | None = None,
    model: str = "",
    in_hit: int = 0,
    in_miss: int = 0,
    out: int = 0,
    reasoning: int = 0,
    cny: float = 0.0,
    status: str = "completed",
) -> ModelTurn:
    """Build a completed neutral turn."""

    del status
    from qqbot.providers.contracts import ModelUsage

    return ModelTurn(
        text=text,
        tool_calls=tuple(tool_calls or ()),
        model=model,
        usage=ModelUsage(in_hit, in_miss, out, reasoning, cny),
    )


def legacy_item(item: PromptItem) -> dict:
    """Readable wire-like shape retained only for existing test assertions."""

    if isinstance(item, Message):
        content = item.content
        if isinstance(content, tuple):
            content = [
                {"type": "input_text", "text": part.text}
                if isinstance(part, TextPart)
                else {"type": "input_image"}
                for part in content
            ]
        return {"role": item.role.value, "content": content}
    if isinstance(item, ToolCall):
        return {
            "type": "function_call",
            "call_id": str(item.call_id),
            "name": item.name,
            "arguments": item.arguments,
        }
    return {
        "type": "function_call_output",
        "call_id": str(item.call_id),
        "output": item.output,
    }


def legacy_tool(tool: ToolSpec) -> dict:
    return {
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
        "strict": tool.strict,
    }


class LegacyTextSession(TextSession):
    """Drive an old-style test callback through the new task-local session API."""

    def __init__(self, model, request) -> None:
        self._model = model
        self._request = request
        self._items = list(request.prompt)
        self._tools = request.tools
        self._policy = request.policy
        self._last: ModelTurn | None = None
        self._closed = False

    async def _call(self) -> ModelTurn:
        if self._closed:
            raise RuntimeError("test session is closed")
        cfg = SimpleNamespace(
            model=self._policy.model,
            reasoning_effort=self._policy.reasoning.value,
            timeout_sec=self._policy.timeout_sec,
            retries=self._policy.retries,
        )
        turn = await self._model.respond(
            [legacy_item(item) for item in self._items],
            cfg=cfg,
            tools=[legacy_tool(tool) for tool in self._tools],
            max_tokens=self._policy.max_output_tokens,
            effort=None,
            kind=self._request.context.purpose.value,
            group_id=self._request.context.group_id,
        )
        self._last = turn
        return turn

    async def start(self) -> ModelTurn:
        if self._last is not None:
            raise RuntimeError("test session already started")
        return await self._call()

    async def continue_with(
        self,
        results: tuple[ToolResult, ...],
        *,
        directive: SessionDirective | None = None,
    ) -> ModelTurn:
        if self._last is None:
            raise RuntimeError("test session has not started")
        self._items.extend(self._last.tool_calls)
        self._items.extend(results)
        if directive is not None:
            self._items.extend(directive.prompt)
            if directive.tools is not None:
                self._tools = directive.tools
            if directive.policy is not None:
                self._policy = directive.policy
        return await self._call()

    async def aclose(self) -> None:
        self._closed = True


class FakeEmbedding(EmbeddingModel):
    """A deterministic vector backend used by the DB-backed suites."""

    name = "fake-embed"
    DIMS = 2048
    EMBED_CALLS = 0

    @property
    def dimensions(self) -> int:
        return self.DIMS

    def rate_for(self, model):
        return Rate("Mtoken", in_miss=0.5)

    async def embed(self, texts, *, cfg=None, group_id=None):
        type(self).EMBED_CALLS += len(texts)
        vectors = []
        for text in texts:
            digest = hashlib.md5(text.encode("utf-8")).digest()
            vectors.append([1.0 + digest[index % 16] / 1275.0 for index in range(self.DIMS)])
        return vectors

    async def aclose(self):
        pass
