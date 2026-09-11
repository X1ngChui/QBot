"""Capability contracts, as abstract base classes.

The layer above depends on *what* it needs - text, vision, ASR, search - never on who
provides it. Nothing in this file names a vendor.

These are ABCs rather than protocols on purpose: backends differ in more than their URL,
and every one of those differences should be forced into a named subclass instead of
accumulating as flags in shared code. Subclassing makes the contract explicit, and an
incomplete backend fails at construction rather than at the first live call.

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
from dataclasses import dataclass, field
from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
from enum import StrEnum

import httpx

from ..settings import AsrCfg, EmbeddingCfg, SearchCfg, TextCfg, VisionCfg, config
from ..util import why

log = logging.getLogger("qqbot.providers")


class Kind(StrEnum):
    """What a billed call was for. Stored as the string, so rows already written keep
    working, and the enum is what code is allowed to name: the ledger's writers and its
    readers share one vocabulary that cannot silently drift apart.
    """

    REPLY = "reply"
    #: The launch checklist's one real call per capability (scripts/preflight.py).
    #: Its own kind so a check run never reads as a reply in /stats.
    PREFLIGHT = "preflight"
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
    delay = min(delay, cap if cap is not None else config().default.llm.retry_after_cap_sec)
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
        retries = config().default.llm.http_retries
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
        effort: str | None = None,
        kind: Kind = Kind.REPLY,
        group_id: str | None = None,
    ) -> ChatResult:
        """Run one completion. Model, deliberation grade and timeout come from `cfg`,
        so a caller with different needs passes a different config rather than a pile
        of exceptions.

        `kind` is what the call is booked as, and it is the enum: a purpose the
        ledger's readers do not know about is a purpose /stats cannot show.

        `effort` overrides cfg.reasoning_effort for the one call, and exists for
        diagnostics that must pin a grade regardless of configuration. A request,
        not a guarantee: backends that cannot deliberate ignore it, and no caller
        may depend on it for correctness - only for cost, latency and depth.
        """

    async def upload(
        self, data: bytes, *, cfg: TextCfg, mime: str = "image/jpeg",
    ) -> str | None:
        """Store a picture with this backend and return an id its messages can carry.

        On the text capability rather than on vision, because the model that reads the
        file block is the one that has to be able to resolve the id. Filed through
        vision, the two would have to share an account for a picture to arrive at
        all - and config invites splitting them, giving each capability its own
        endpoint and credential.

        Not abstract: keeping files is a backend feature, not part of the contract.
        None means this backend keeps none, and the caller then leaves originals out
        of the prompt and lets the description line stand alone.
        """
        return None


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
    """The five capabilities, injected as one bundle.

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

    async def aclose(self) -> None:
        """Close all five, even if one refuses: a backend that raises on the way out
        must not leave the others holding their connections."""
        for cap in (self.text, self.vision, self.asr, self.embedding, self.search):
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
