"""Capability wiring and provider-neutral public contracts."""

from __future__ import annotations

from .base import (
    AsrModel,
    Capability,
    EmbeddingModel,
    Kind,
    PageReader,
    Providers,
    SearchEngine,
    TextModel,
    TextSession,
    VisionModel,
)
from .contracts import (
    CallContext,
    CallPurpose,
    GenerationPolicy,
    Message,
    ModelFailure,
    ModelRequest,
    ModelTurn,
    ModelUsage,
    Role,
    SessionDirective,
    ToolCall,
    ToolCallId,
    ToolResult,
    ToolSpec,
)

__all__ = [
    "AsrModel",
    "CallContext",
    "CallPurpose",
    "Capability",
    "EmbeddingModel",
    "GenerationPolicy",
    "Kind",
    "Message",
    "ModelFailure",
    "ModelRequest",
    "ModelTurn",
    "ModelUsage",
    "PageReader",
    "Providers",
    "Role",
    "SearchEngine",
    "SessionDirective",
    "TextModel",
    "TextSession",
    "ToolCall",
    "ToolCallId",
    "ToolResult",
    "ToolSpec",
    "VisionModel",
    "build_default",
    "providers",
    "set_providers",
]

_providers: Providers | None = None


def build_default() -> Providers:
    from ..settings import config
    from .registry import build

    return build(config().default)


def providers() -> Providers:
    global _providers
    if _providers is None:
        _providers = build_default()
    return _providers


def set_providers(bundle: Providers | None) -> None:
    """Inject a bundle for tests; None restores startup construction."""
    global _providers
    _providers = bundle
