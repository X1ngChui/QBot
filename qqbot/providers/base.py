"""Capability contracts, as abstract base classes.

The layer above depends on *what* it needs - text, vision, ASR, search - never on who
provides it. Nothing in this file names a vendor.

Lifecycle-bearing capabilities are ABCs: they own clients, concurrency or native
resources, and an incomplete implementation should fail at construction. Narrow
collaborators with no independent lifecycle, such as PageReader, use Protocol instead.

One capability may be repointed at another platform without touching the others.
There is no automatic fallback between backends: a silent downgrade would answer
from a model nobody chose, at a price nobody budgeted.
"""

from __future__ import annotations

import asyncio
import logging
import random
from abc import ABC, abstractmethod
from collections.abc import Awaitable, Callable, Coroutine, Mapping
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Protocol
from email.utils import parsedate_to_datetime

import httpx

from ..settings import AsrCfg, EmbeddingCfg, SearchCfg, VisionCfg, config
from ..util import why
from .contracts import (
    AttachmentStore,
    CallPurpose,
    ModelRequest,
    ModelTurn,
    SessionDirective,
    ToolResult,
)

log = logging.getLogger("qqbot.providers")

Kind = CallPurpose


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


# -- retrying ---------------------------------------------------------------



def retry_after_seconds(headers: Mapping[str, str]) -> float | None:
    """The Retry-After header as seconds, or None when absent or unreadable.

    Both spellings the header allows: a delay in seconds, or an HTTP-date; a date
    already past is zero, not negative.
    """
    raw = (headers.get("retry-after") or "").strip()
    if not raw:
        return None
    if raw.isdigit():
        return float(raw)
    try:
        when = parsedate_to_datetime(raw)
    except (TypeError, ValueError):
        return None
    if when.tzinfo is None:
        when = when.replace(tzinfo=UTC)
    return max(0.0, (when - datetime.now(UTC)).total_seconds())


def backoff_delay(attempt: int, *, retry_after: float | None = None,
                  cap: float | None = None) -> float:
    """How long to sleep before retry number `attempt` (1-based).

    The base schedule is short and doubling (0.5 s, 1 s, 2 s ...): a connection
    reset or a 5xx is usually gone by the next second. A vendor that names its own
    wait (Retry-After on a 429) is honoured up to the cap, since retrying before it
    only burns the attempt. Jitter of up to a quarter keeps every caller that hit
    the same limit from returning in one wave.
    """
    delay = 0.5 * 2 ** (attempt - 1)
    if retry_after is not None:
        delay = max(delay, retry_after)
    ceiling = cap if cap is not None else config().default.capabilities.retry_after_cap_sec
    delay = min(delay, ceiling)
    return delay + random.uniform(0.0, 0.25 * delay)


def _http_retryable(e: Exception) -> bool:
    # Transport failures never reached a reply; a 429 or a 5xx is the vendor's own
    # request to try again. A 4xx of any other kind is the request's fault and is
    # the same request next time.
    if isinstance(e, httpx.TransportError):
        return True
    if isinstance(e, httpx.HTTPStatusError):
        status = e.response.status_code
        return status == 429 or status >= 500
    return False


async def with_retry[T](
    fn: Callable[[], Awaitable[T]], *, what: str,
    retries: int | None = None, retry_after_cap: float | None = None,
) -> T:
    """Run `fn` again on a transport error, a 429 or a 5xx, up to `retries` times.

    For the backends that speak plain httpx rather than the OpenAI SDK, so that a
    flaky proxy or a momentary rate limit costs a short sleep instead of the whole
    call. `fn` must raise httpx.HTTPStatusError itself (raise_for_status) for the
    status rule to see it. Anything else propagates on the first attempt.
    """
    if retries is None:
        retries = config().default.capabilities.http_retries
    attempt = 0
    while True:
        try:
            return await fn()
        except (httpx.TransportError, httpx.HTTPStatusError) as e:
            if not _http_retryable(e):
                raise
            attempt += 1
            if attempt > retries:
                raise
            wait = (retry_after_seconds(e.response.headers)
                    if isinstance(e, httpx.HTTPStatusError) else None)
            delay = backoff_delay(attempt, retry_after=wait, cap=retry_after_cap)
            log.info("%s failed (%s), retry %d/%d in %.1fs", what, why(e),
                     attempt, retries, delay)
            await asyncio.sleep(delay)


class QuotaExhausted(RuntimeError):
    """A capability's free allowance for the period is used up.

    Raised instead of billing, and it must propagate: a limit reached means the reply
    is dropped, never answered in degraded form - so a backend must raise this rather
    than return an in-band message, and no handler between here and the engine may
    swallow it. Distinct from a transport failure, which stays a tool answer: a broken
    network is an error to talk around, not a limit to respect."""


@dataclass(frozen=True)
class Rate:
    """What one unit of a capability costs, in CNY.

    Prices belong to the backend that charges them, not to a shared table: only
    DeepSeekText knows what DeepSeek bills, and only the search backend knows what a
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


class TextSession(ABC):
    """One linear model run, owned by exactly one agent task."""

    @abstractmethod
    async def start(self) -> ModelTurn:
        """Produce the first completed turn."""

    @abstractmethod
    async def continue_with(
        self,
        results: tuple[ToolResult, ...],
        *,
        directive: SessionDirective | None = None,
    ) -> ModelTurn:
        """Continue after one result per call.

        A directive appends neutral instructions and may narrow tools or policy for
        a send-only wrap-up. Provider replay state remains private to the session.
        """

    @abstractmethod
    async def aclose(self) -> None:
        """Release task-local state. Safe to call after completion or failure."""

    async def __aenter__(self) -> TextSession:
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.aclose()


class Capability(ABC):
    """Common lifecycle and pricing. Every backend holds a client and knows its own rates."""

    #: Short name this backend is selected by in config.
    name: str = ""
    #: Whether this backend authenticates at all. An in-process backend has no
    #: endpoint and no key, and a launch check must not report its missing one.
    needs_key: bool = True

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
    """A factory for isolated, task-local model sessions."""

    @abstractmethod
    def open_session(self, request: ModelRequest) -> TextSession:
        """Create a fresh session; it must not share continuation state."""

    #: Optional provider-owned storage used by open_images. The core sees only an
    #: opaque StoredImage and never a vendor file identifier or expiry rule.
    attachments: AttachmentStore | None = None


class VisionModel(Capability):
    """Image understanding. Implementations must accept the bytes inline: media never
    touches disk, so a backend that only takes a public URL cannot satisfy this."""

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


class AsrModel(Capability):
    """Speech to text. Inline bytes, for the same reason as VisionModel."""

    async def start(self, cfg: AsrCfg) -> None:
        """Load restart-scoped resources before the gateway accepts messages."""

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


class EmbeddingModel(Capability):
    """Text in, vectors out.

    Batching is the implementation's business: endpoints cap how many inputs one
    request may carry, and a caller that had to know would be knowing a vendor.
    """

    @abstractmethod
    async def embed(self, texts, *, cfg: EmbeddingCfg,
                    group_id: str | None = None) -> list[list[float]]:
        """Vectors for these texts, in the order they were given."""


class SearchEngine(Capability):
    """Web search, called directly rather than through a model's built-in search."""

    @abstractmethod
    async def search(
        self, query: str, *, cfg: SearchCfg, group_id: str | None = None
    ) -> list[dict]:
        """Return at most cfg.count results, each normalised to {title, link, content}."""


class PageReader(Protocol):
    """Optional narrow capability for extracting readable text from one URL."""

    async def read_page(
        self, url: str, *, cfg: SearchCfg, group_id: str | None = None
    ) -> str:
        """Return readable page text, or an empty string when none is available."""


@dataclass
class Providers:
    """Lifecycle capabilities plus explicitly injected optional collaborators.

    Tests hand over fakes; a different backend is a different object here. Neither case
    touches the code that uses them. Every capability belongs in the bundle: one wired
    separately is one that shutdown, /reload and the startup log all have to be told
    about by hand, and each of those is a place to forget it.
    """

    text: TextModel
    vision: VisionModel
    asr: AsrModel
    embedding: EmbeddingModel
    search: SearchEngine
    page_reader: PageReader | None = None

    async def aclose(self) -> None:
        """Close every lifecycle capability, once, even when one close fails."""
        seen: set[int] = set()
        for cap in (self.text, self.vision, self.asr, self.embedding, self.search):
            if id(cap) in seen:
                continue
            seen.add(id(cap))
            try:
                await cap.aclose()
            except Exception:
                log.warning("closing %s failed", type(cap).__name__, exc_info=True)

    def describe(self) -> str:
        return (
            f"text={self.text.name or type(self.text).__name__} "
            f"vision={self.vision.name or type(self.vision).__name__} "
            f"asr={self.asr.name or type(self.asr).__name__} "
            f"embedding={self.embedding.name or type(self.embedding).__name__} "
            f"search={self.search.name or type(self.search).__name__}"
        )
