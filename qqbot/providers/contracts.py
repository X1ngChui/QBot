"""Provider-neutral values exchanged by the agent and model capabilities.

The model boundary is deliberately smaller than any vendor API.  Provider wire items,
response identifiers, reasoning payloads and SDK exceptions belong to a session adapter;
the agent sees only completed turns and supplies typed tool results.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any, NewType, Protocol

type JsonScalar = str | int | float | bool | None
type JsonValue = JsonScalar | list[JsonValue] | dict[str, JsonValue]
type JsonObject = Mapping[str, JsonValue]
ToolCallId = NewType("ToolCallId", str)


class Role(StrEnum):
    SYSTEM = "system"
    DEVELOPER = "developer"
    USER = "user"
    ASSISTANT = "assistant"


class CallPurpose(StrEnum):
    REPLY = "reply"
    PREFLIGHT = "preflight"
    EXTRACT = "extract"
    SEARCH = "search"
    VISION = "vision"
    ASR = "asr"
    EMBED = "embed"


class ReasoningEffort(StrEnum):
    OFF = "off"
    LOW = "low"
    HIGH = "high"
    MAX = "max"


class FailureKind(StrEnum):
    UNAVAILABLE = "unavailable"
    REJECTED = "rejected"
    INCOMPLETE = "incomplete"
    PROTOCOL = "protocol"


class ChargeState(StrEnum):
    NOT_SENT = "not_sent"
    NOT_CHARGED = "not_charged"
    MAY_HAVE_CHARGED = "may_have_charged"
    CHARGED = "charged"


@dataclass(frozen=True, slots=True)
class TextPart:
    text: str


@dataclass(frozen=True, slots=True)
class ImageBytes:
    data: bytes
    media_type: str


@dataclass(frozen=True, slots=True)
class StoredImage:
    """An opaque image handle issued by the selected text capability."""

    provider: str
    handle: str


class AttachmentStore(Protocol):
    async def store(self, data: bytes, media_type: str) -> StoredImage:
        """Return an opaque handle scoped to this provider account."""

    async def aclose(self) -> None:
        """Release upload resources."""


type InputPart = TextPart | ImageBytes | StoredImage


@dataclass(frozen=True, slots=True)
class Message:
    role: Role
    content: str | tuple[InputPart, ...]


@dataclass(frozen=True, slots=True)
class ToolCall:
    call_id: ToolCallId
    name: str
    arguments: str


type ToolOutput = str | tuple[InputPart, ...]


@dataclass(frozen=True, slots=True)
class ToolResult:
    call_id: ToolCallId
    output: ToolOutput


type PromptItem = Message | ToolCall | ToolResult


@dataclass(frozen=True, slots=True)
class ToolSpec:
    name: str
    description: str
    parameters: JsonObject
    strict: bool = False


@dataclass(frozen=True, slots=True)
class ModelUsage:
    input_cached: int = 0
    input_uncached: int = 0
    output: int = 0
    reasoning: int = 0
    cny: float = 0.0
    estimated: bool = False


@dataclass(frozen=True, slots=True)
class ModelTurn:
    text: str = ""
    tool_calls: tuple[ToolCall, ...] = ()
    model: str = ""
    usage: ModelUsage = field(default_factory=ModelUsage)


@dataclass(frozen=True, slots=True)
class GenerationPolicy:
    model: str
    reasoning: ReasoningEffort = ReasoningEffort.OFF
    timeout_sec: float = 30.0
    retries: int = 2
    max_output_tokens: int | None = None


@dataclass(frozen=True, slots=True)
class CallContext:
    purpose: CallPurpose = CallPurpose.REPLY
    group_id: str | None = None


@dataclass(frozen=True, slots=True)
class SessionDirective:
    prompt: tuple[PromptItem, ...] = ()
    tools: tuple[ToolSpec, ...] | None = None
    policy: GenerationPolicy | None = None


@dataclass(frozen=True, slots=True)
class ModelRequest:
    prompt: tuple[PromptItem, ...]
    tools: tuple[ToolSpec, ...]
    policy: GenerationPolicy
    context: CallContext = field(default_factory=CallContext)


class ModelFailure(RuntimeError):
    """A provider-neutral model failure with retry and charging semantics."""

    def __init__(
        self,
        message: str,
        *,
        kind: FailureKind,
        charge: ChargeState,
        retryable: bool = False,
    ) -> None:
        super().__init__(message)
        self.kind = kind
        self.charge = charge
        self.retryable = retryable


def json_object(value: Mapping[str, Any]) -> JsonObject:
    """Narrow a validated JSON-schema mapping for ToolSpec construction."""

    return value  # type: ignore[return-value]
