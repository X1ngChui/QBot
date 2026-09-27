"""Capability wiring and provider-neutral public contracts."""

from __future__ import annotations

from qqbot.providers.base import AsrModel
from qqbot.providers.base import Capability
from qqbot.providers.base import EmbeddingModel
from qqbot.providers.base import Kind
from qqbot.providers.base import PageReader
from qqbot.providers.base import Providers
from qqbot.providers.base import SearchEngine
from qqbot.providers.base import TextModel
from qqbot.providers.base import TextSession
from qqbot.providers.base import VisionModel
from qqbot.providers.contracts import CallContext
from qqbot.providers.contracts import CallPurpose
from qqbot.providers.contracts import GenerationPolicy
from qqbot.providers.contracts import Message
from qqbot.providers.contracts import ModelFailure
from qqbot.providers.contracts import ModelRequest
from qqbot.providers.contracts import ModelTurn
from qqbot.providers.contracts import ModelUsage
from qqbot.providers.contracts import Role
from qqbot.providers.contracts import SessionDirective
from qqbot.providers.contracts import ToolCall
from qqbot.providers.contracts import ToolCallId
from qqbot.providers.contracts import ToolResult
from qqbot.providers.contracts import ToolSpec

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
]
