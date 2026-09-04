"""Capability contracts, as abstract base classes.

The layer above depends on *what* it needs - text, vision, ASR, search - never on who
provides it. Nothing in this file names a vendor.

These are ABCs rather than protocols on purpose: backends differ in more than their URL,
and every one of those differences should be forced into a named subclass instead of
accumulating as flags in shared code. Subclassing makes the contract explicit, and an
incomplete backend fails at construction rather than at the first live call.

Section 5.1 splits providers by capability so each can move independently; D6 rules out
automatic fallback between them.
"""

from __future__ import annotations

import asyncio
from abc import ABC, abstractmethod
from collections.abc import Coroutine
from dataclasses import dataclass, field
from enum import StrEnum

from ..settings import AsrCfg, SearchCfg, TextCfg, VisionCfg


class Kind(StrEnum):
    """What a billed call was for. Stored as the string, so rows already written keep
    working, and the enum is what code is allowed to name: the ledger's writers and its
    readers share one vocabulary that cannot silently drift apart.
    """

    REPLY = "reply"
    #: Reading a batch of transcript into memory candidates. Its own kind because it is
    #: the one recurring cost that scales with how much the group talks rather than with
    #: how often the bot answers, and the two need to be readable apart.
    EXTRACT = "extract"
    SEARCH = "search"
    VISION = "vision"
    ASR = "asr"
    #: Vector projections. Cheap per call, but a paid capability that never reached
    #: the ledger read exactly like a quiet day - the daily cap, /stats and the
    #: report all measure only what is booked.
    EMBED = "embed"


#: Strong references to in-flight retirement tasks: asyncio holds tasks weakly,
#: so a bare create_task can be collected mid-close and leak the pool it was
#: closing.
_RETIRING: set[asyncio.Task] = set()


def retire(closing: Coroutine) -> None:
    """Run a replaced client's close coroutine in the background.

    For the lazily-rebuilt clients (endpoint or proxy changed under /reload):
    the caller is mid-request and cannot await the old pool's close, but
    dropping it unclosed leaks its connections for the process lifetime. No
    running loop means a test context with nothing open.
    """
    try:
        task = asyncio.get_running_loop().create_task(closing)
    except RuntimeError:
        closing.close()
        return
    _RETIRING.add(task)
    task.add_done_callback(_RETIRING.discard)


class QuotaExhausted(RuntimeError):
    """A capability's free allowance for the period is used up.

    Raised instead of billing, and it must propagate: a limit reached means the reply
    is dropped (the owner's rule), never answered in degraded form - so a backend must
    raise this rather than return an in-band message, and no handler between here and
    the engine may swallow it. Distinct from a transport failure, which stays a tool
    answer: a broken network is an error to talk around, not a limit to respect."""


@dataclass(frozen=True)
class Rate:
    """What one unit of a capability costs, in CNY.

    Prices belong to the backend that charges them, not to a shared table: only
    DeepSeekChat knows what DeepSeek bills, and only the search backend knows what a
    call against its allowance is worth. The upper layer never sees a vendor - it asks
    the capability for a Rate and multiplies.

    `unit` says which fields are meaningful: "Mtoken" uses in_hit/in_miss/out, everything
    else uses per_unit.
    """

    unit: str
    in_hit: float = 0.0
    in_miss: float = 0.0
    out: float = 0.0
    per_unit: float = 0.0
    source: str = ""

    def tokens(self, in_hit: int, in_miss: int, out: int) -> float:
        return (in_hit * self.in_hit + in_miss * self.in_miss + out * self.out) / 1_000_000

    def units(self, n: float) -> float:
        return n * self.per_unit


@dataclass
class ChatResult:
    """What a text model returns, normalised across backends.

    Token counts feed the budget gate. `out` is what bills; `reasoning` is the share of it
    a deliberating backend spent thinking, broken out only so the cost is visible. Backends
    that do not deliberate leave it at zero.
    """

    text: str = ""
    tool_calls: list[dict] = field(default_factory=list)
    model: str = ""
    in_hit: int = 0
    in_miss: int = 0
    out: int = 0
    reasoning: int = 0
    finish_reason: str = ""
    cny: float = 0.0

    @property
    def truncated(self) -> bool:
        return self.finish_reason == "length"


class Capability(ABC):
    """Common lifecycle and pricing. Every backend holds a client and knows its own rates."""

    #: Short name this backend is selected by in config.
    name: str = ""

    @abstractmethod
    async def aclose(self) -> None:
        """Release any connections. Must be safe to call twice."""

    @abstractmethod
    def rate_for(self, model: str) -> Rate:
        """What this backend charges for the given model, right now.

        Abstract on purpose: a new backend that forgets to price itself fails at
        construction rather than silently billing at someone else's rates. Unknown models
        should return a deliberately pessimistic guess - overestimating trips the budget
        gate early, which is the safe direction.

        The answer may depend on the clock - DeepSeek bills weekday working hours at
        double - so this is asked at the moment a call is booked and its result is not
        cached anywhere.
        """


class TextModel(Capability):
    """Group replies, arbitration, summaries and profile rewrites."""

    @abstractmethod
    async def chat(
        self,
        messages: list[dict],
        *,
        cfg: TextCfg,
        tools: list[dict] | None = None,
        max_tokens: int | None = None,
        timeout: float | None = None,
        effort: str | None = None,
        kind: str = "reply",
        group_id: str | None = None,
    ) -> ChatResult:
        """Run one completion.

        `effort` is the deliberation grade for this call - "off", "low", "high" or
        "max" - overriding cfg.reasoning_effort when set; None means the config
        decides. A request, not a guarantee: backends that cannot deliberate ignore
        it, and callers must not depend on it for correctness - only for cost,
        latency and answer depth.
        """


class VisionModel(Capability):
    """Image understanding. Implementations must accept the bytes inline: media never
    touches disk (5.3), so a backend that only takes a public URL cannot satisfy this."""

    @abstractmethod
    async def describe(
        self,
        data: bytes,
        *,
        cfg: VisionCfg,
        prompt: str,
        mime: str = "image/jpeg",
        group_id: str | None = None,
    ) -> str:
        """Return one or two sentences describing the image, or "" if there is nothing.

        The prompt comes from the caller, not from config: what to ask about a picture
        is part of how the archive is built, and lives with the prompt files
        (config/prompts, key "describe_image").
        """


    async def upload(
        self, data: bytes, *, cfg: VisionCfg, mime: str = "image/jpeg",
    ) -> str | None:
        """Store the image with the backend and return an id a chat message can carry.

        Not abstract: keeping files is a backend feature, not part of the contract.
        None means this backend keeps none - the caller then leaves raw images out of
        the prompt and the text description stands alone, which is the pre-vision
        behaviour and always correct.
        """
        return None


class AsrModel(Capability):
    """Speech to text. Inline bytes, for the same reason as VisionModel."""

    @abstractmethod
    async def transcribe(
        self,
        data: bytes,
        *,
        cfg: AsrCfg,
        fmt: str = "wav",
        seconds: float | None = None,
        group_id: str | None = None,
    ) -> str:
        """Return the transcript, or "" if nothing was said."""


class SearchEngine(Capability):
    """Web search, called directly rather than through a model's built-in search (D5)."""

    @abstractmethod
    async def search(
        self, query: str, *, cfg: SearchCfg, group_id: str | None = None
    ) -> list[dict]:
        """Return at most cfg.count results, each normalised to {title, link, content}."""

    async def extract(
        self, url: str, *, cfg: SearchCfg, group_id: str | None = None
    ) -> str:
        """The readable text of one page, or "" when the page yields none.

        Not abstract: a search backend without page extraction is still a search
        backend, and the tool layer answers "unavailable" for it. Shares the search
        allowance where the vendor meters both from one pool."""
        raise NotImplementedError(f"{self.name} cannot read pages")


@dataclass
class Providers:
    """The four capabilities, injected as one bundle.

    Tests hand over fakes; a different backend is a different object here. Neither case
    touches the code that uses them.
    """

    text: TextModel
    vision: VisionModel
    asr: AsrModel
    search: SearchEngine

    async def aclose(self) -> None:
        for cap in (self.text, self.vision, self.asr, self.search):
            await cap.aclose()

    def describe(self) -> str:
        return (
            f"text={self.text.name or type(self.text).__name__} "
            f"vision={self.vision.name or type(self.vision).__name__} "
            f"asr={self.asr.name or type(self.asr).__name__} "
            f"search={self.search.name or type(self.search).__name__}"
        )
