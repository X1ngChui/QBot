"""Generic implementations for backends speaking the OpenAI chat protocol.

These are usable as-is against a plain OpenAI-compatible endpoint, and serve as the base
for the vendor subclasses in the sibling modules. Everything a backend is likely to differ
on is an overridable hook rather than a flag:

    _usage_tokens      how usage is reported (cache-hit split, reasoning tokens)
    _terse_body        how to ask it not to deliberate, if it can be asked
    _extra_body        anything else the request needs
    _read_content      how the answer is carried on the message

Anything a subclass cannot express through those hooks is a sign it is not really this
protocol, and it should implement the ABC directly.
"""

from __future__ import annotations

import asyncio
import base64
import logging
from typing import Any

import openai
from openai import AsyncOpenAI

from ..core.budget import BUDGET
from ..settings import AsrCfg, TextCfg, VisionCfg
from ..util import read_api_key, require_key, why
from .base import (
    AsrModel, ChatResult, Kind, Rate, TextModel, VisionModel, backoff_delay, retire,
    retry_after_seconds,
)

log = logging.getLogger("qqbot.llm")

# A generic endpoint's prices are unknowable from here, so they are guessed high. Billing
# a real backend at these rates would be wrong - that is what the vendor subclasses are
# for - but overestimating only trips the budget gate early, which beats spending money
# the gate thinks was never spent.
UNKNOWN_TOKEN_RATE = Rate("Mtoken", in_hit=3.0, in_miss=3.0, out=6.0, source="unpriced backend")
UNKNOWN_SECOND_RATE = Rate("second", per_unit=0.001, source="unpriced backend")

RETRYABLE = (
    openai.APIConnectionError,
    openai.APITimeoutError,
    openai.RateLimitError,
    openai.InternalServerError,
    asyncio.TimeoutError,
)

#: Failures that provably cost nothing: the request never left the machine, or the
#: vendor refused it before processing. Callers that treat a paid call's failure as
#: terminal (media abandons a voice clip after one billed attempt) may keep
#: retrying these - the billing boundary is the vendor taxonomy's to know, which
#: is why this lives here and not with the caller.
NEVER_BILLED = (openai.APIConnectionError, openai.RateLimitError)


#: What the SDK is handed when an endpoint checks no credential at all. It refuses an
#: empty string, and a self-hosted model has no key to give it.
NO_KEY_PLACEHOLDER = "local"


async def _book_timeout(*, kind: Kind, model: str, cny: float, group_id: str | None,
                        in_miss: int = 0, out: int = 0) -> None:
    """Book a call that timed out, at what the vendor charged for it.

    A timed-out attempt is not a free one: the request reached the vendor, which
    billed the prompt (or the clip) on arrival and everything generated up to the
    cut - but usage travels in the response, which a timeout never reads. Left
    unbooked this would be the one path that understates spend, in a module whose
    every other guess leans high, so each caller books an estimate that leans high
    too. Callers catch both timeout shapes: the outer wait_for and the SDK's own
    APITimeoutError, which can fire first because the request carries the same
    deadline - half of all timeouts would otherwise slip through unbooked.
    """
    await BUDGET.record(kind=kind, model=model, cny=cny, group_id=group_id,
                        in_miss=in_miss, out=out)


class OpenAIClient:
    """Lazily built AsyncOpenAI client, rebuilt if the endpoint or credential changes.

    The identity is the endpoint and the resolved key value - not the variable's name:
    a key rotated in place would otherwise keep serving the client built on the old
    value until a restart, while the plain-HTTP backends read theirs on every call.
    Timeout is deliberately not part of it: two configs that share an account (the
    reply path and its extraction copy) differ only in timeout, and keying on it would
    rebuild the pool every time they alternate. Callers pass the deadline per request.
    """

    def __init__(self) -> None:
        self._client: AsyncOpenAI | None = None
        self._id: tuple[str, str] = ("", "")

    def api(self, *, base_url: str, api_key_env: str, timeout: float, what: str,
            key_required: bool = True) -> AsyncOpenAI:
        if key_required:
            key = require_key(api_key_env, what)
        else:
            key = read_api_key(api_key_env) or NO_KEY_PLACEHOLDER
        ident = (base_url, key)
        if self._client is None or self._id != ident:
            if self._client is not None:
                retire(self._client.close())
            self._client = AsyncOpenAI(
                base_url=base_url, api_key=key, timeout=timeout, max_retries=0
            )
            self._id = ident
        return self._client

    async def aclose(self) -> None:
        if self._client is not None:
            await self._client.close()
            self._client = None
            self._id = ("", "")


# -- text -------------------------------------------------------------------


class OpenAICompatChat(TextModel):
    """Streaming chat with tool-call accumulation. No fallback, no downgrade: on
    failure this raises and the caller decides to stay silent."""

    name = "openai_compat"
    #: Whether an empty credential is a configuration error. A hosted endpoint
    #: without a key answers 401 on every call, so failing early names the cause; a
    #: self-hosted one may check nothing, and its subclass turns this off.
    key_required: bool = True

    def __init__(self) -> None:
        self._conn = OpenAIClient()
        self._sem: asyncio.Semaphore | None = None
        self._sem_size = 0

    async def aclose(self) -> None:
        await self._conn.aclose()

    def rate_for(self, model: str) -> Rate:
        return UNKNOWN_TOKEN_RATE

    # -- hooks ------------------------------------------------------------
    def _usage_tokens(self, usage: dict) -> tuple[int, int, int, int]:
        """(in_hit, in_miss, out, reasoning). Default: no cache reporting, so bill
        everything as a miss - the safe direction."""
        total_in = usage.get("prompt_tokens", 0) or 0
        return 0, total_in, usage.get("completion_tokens", 0) or 0, 0

    def _terse_body(self) -> dict[str, Any]:
        """How to ask this backend not to deliberate. Empty means it cannot be asked."""
        return {}

    def _extra_body(self, *, cfg: TextCfg, effort: str) -> dict[str, Any]:
        """Request extras for the resolved deliberation grade. The generic protocol
        has no grade field, so all it can express is off (via _terse_body, for
        backends that can be asked not to deliberate) versus default behaviour."""
        return dict(self._terse_body()) if effort == "off" else {}

    def _image_part(self, image_id: str) -> dict[str, Any]:
        """One attached picture, in this backend's own wire shape.

        The layer above says {"type": "image", "id": ...} and nothing more: which
        JSON a vendor wants for a stored picture is exactly the kind of quirk this
        class exists to hold, and the shapes really do differ - DeepSeek takes a flat
        file_id and rejects the nesting below, which is what OpenAI documents.
        """
        return {"type": "file", "file": {"file_id": image_id}}

    def _wire(self, messages: list[dict]) -> list[dict]:
        """The conversation with its neutral image markers translated for this vendor.

        Copied, never mutated: the caller keeps one message list across the whole
        tool loop and re-sends it every round, so rewriting in place would translate
        the same part again on the next pass.
        """
        out = []
        for m in messages:
            content = m.get("content")
            if not isinstance(content, list) or not any(
                    isinstance(p, dict) and p.get("type") == "image" for p in content):
                out.append(m)
                continue
            out.append({**m, "content": [
                self._image_part(p["id"])
                if isinstance(p, dict) and p.get("type") == "image" else p
                for p in content
            ]})
        return out

    # -- driver -----------------------------------------------------------
    def _gate(self, cfg: TextCfg) -> asyncio.Semaphore:
        # One limit for every caller of this backend. Resizing under load would lose
        # outstanding permits, so a changed value applies at the next start.
        if self._sem is None:
            self._sem = asyncio.Semaphore(cfg.max_concurrency)
            self._sem_size = cfg.max_concurrency
        elif self._sem_size != cfg.max_concurrency:
            log.info("max_concurrency %d -> %d applies after restart",
                     self._sem_size, cfg.max_concurrency)
            self._sem_size = cfg.max_concurrency
        return self._sem

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
        model = cfg.model
        attempt = 0
        while True:
            try:
                async with self._gate(cfg):
                    res = await asyncio.wait_for(
                        self._stream_once(
                            messages, cfg=cfg, model=model, tools=tools,
                            max_tokens=max_tokens,
                            effort=effort if effort is not None else cfg.reasoning_effort,
                        ),
                        timeout=cfg.timeout_sec,
                    )
                break
            except RETRYABLE as e:
                if isinstance(e, (asyncio.TimeoutError, openai.APITimeoutError)):
                    # The estimate: the prompt's rendered characters as miss-rate
                    # input tokens (at least one token per character for Chinese, so
                    # never an undercount), the full output allowance as output -
                    # possibly thousands of reasoning tokens went into the cut.
                    # Booked per attempt, because each retry spends again.
                    est_in = sum(len(str(m)) for m in messages)
                    est_out = max_tokens or 4096
                    await _book_timeout(
                        kind=kind, model=model,
                        cny=self.rate_for(model).tokens(0, est_in, est_out),
                        group_id=group_id, in_miss=est_in, out=est_out,
                    )
                attempt += 1
                if attempt > cfg.retries:
                    log.warning("text model %s out of retries: %s", model, why(e))
                    raise
                # A rate limit names its own wait; everything else takes the short
                # schedule, since a dropped connection or a 5xx is usually over by
                # the next second.
                wait = (retry_after_seconds(e.response.headers)
                        if isinstance(e, openai.RateLimitError) else None)
                await asyncio.sleep(backoff_delay(attempt, retry_after=wait))

        if res.truncated and not res.text:
            # Silent-failure guard: a deliberating backend can spend the whole output
            # budget thinking and return nothing. Without this the caller just sees
            # "the model said nothing".
            log.warning(
                "%s call to %s hit max_tokens (%s) with empty content, %d of it reasoning",
                kind, model, max_tokens, res.reasoning,
            )

        billed = res.model or model
        res.cny = await BUDGET.record(
            kind=kind, model=billed,
            cny=self.rate_for(billed).tokens(res.in_hit, res.in_miss, res.out),
            group_id=group_id,
            in_hit=res.in_hit, in_miss=res.in_miss, out=res.out,
        )
        return res

    async def _stream_once(
        self,
        messages: list[dict],
        *,
        cfg: TextCfg,
        model: str,
        tools: list[dict] | None,
        max_tokens: int | None,
        effort: str,
    ) -> ChatResult:
        kwargs: dict[str, Any] = {
            "model": model,
            "messages": self._wire(messages),
            "stream": True,
            "stream_options": {"include_usage": True},
        }
        if tools:
            kwargs["tools"] = tools
        if max_tokens:
            kwargs["max_tokens"] = max_tokens
        extra = self._extra_body(cfg=cfg, effort=effort)
        if extra:
            kwargs["extra_body"] = extra

        out = ChatResult(model=model)
        parts: list[str] = []
        calls: dict[int, dict] = {}

        client = self._conn.api(
            base_url=cfg.base_url, api_key_env=cfg.api_key_env,
            timeout=cfg.timeout_sec, what="text", key_required=self.key_required,
        )
        # Per request, always. The client is cached by endpoint and credential, so its
        # own timeout is whichever config built it first - and a background batch
        # reading a hundred messages shares that client with a reply somebody is
        # waiting for. Without this the SDK would abort at the other use's deadline
        # and the outer wait_for could never extend anything.
        kwargs["timeout"] = cfg.timeout_sec
        async for chunk in await client.chat.completions.create(**kwargs):
            # What actually served the request, which is not always what was asked
            # for: a vendor may retire an id and route it to its successor. The
            # ledger and the price lookup follow the served model, so a routed
            # call bills at the rate it was really charged at - and an unpriced
            # successor announces itself through the price table's warning
            # instead of hiding inside a familiar name.
            if chunk.model:
                out.model = chunk.model
            if chunk.usage:
                out.in_hit, out.in_miss, out.out, out.reasoning = self._usage_tokens(
                    chunk.usage.model_dump()
                )
            if not chunk.choices:
                continue
            if chunk.choices[0].finish_reason:
                out.finish_reason = chunk.choices[0].finish_reason
            delta = chunk.choices[0].delta
            if delta is None:
                continue
            # A deliberating backend carries its thinking on a separate field. It is never
            # accumulated: it must not reach the group.
            if delta.content:
                parts.append(delta.content)
            for tc in delta.tool_calls or []:
                idx = tc.index
                if idx is None:
                    # Not every backend numbers its deltas. Without an index, a
                    # delta carrying an unseen id opens the next slot; one carrying
                    # no id continues the latest. A None key would sort against the
                    # integers below and fail the whole reply.
                    known = next((i for i, c in calls.items() if tc.id and c["id"] == tc.id),
                                 None)
                    if known is not None:
                        idx = known
                    elif tc.id or not calls:
                        idx = max(calls, default=-1) + 1
                    else:
                        idx = max(calls)
                slot = calls.setdefault(
                    idx,
                    {"id": "", "type": "function", "function": {"name": "", "arguments": ""}},
                )
                if tc.id:
                    slot["id"] = tc.id
                if tc.function and tc.function.name:
                    slot["function"]["name"] = tc.function.name
                if tc.function and tc.function.arguments:
                    slot["function"]["arguments"] += tc.function.arguments

        out.text = "".join(parts).strip()
        out.tool_calls = [calls[i] for i in sorted(calls)]
        return out


# -- vision -----------------------------------------------------------------


class OpenAICompatVision(VisionModel):
    """Image understanding via a chat completion carrying an inline data URI."""

    name = "openai_compat"

    def __init__(self) -> None:
        self._conn = OpenAIClient()

    async def aclose(self) -> None:
        await self._conn.aclose()

    def rate_for(self, model: str) -> Rate:
        return UNKNOWN_TOKEN_RATE

    def _extra_body(self, cfg: VisionCfg) -> dict[str, Any]:
        return {}

    async def describe(
        self, data: bytes, *, cfg: VisionCfg, prompt: str, mime: str = "image/jpeg",
        group_id: str | None = None,
    ) -> str:
        b64 = base64.b64encode(data).decode()
        kwargs: dict[str, Any] = {
            "model": cfg.model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "image_url", "image_url": {"url": f"data:{mime};base64,{b64}"}},
                        {"type": "text", "text": prompt.strip()},
                    ],
                }
            ],
        }
        extra = self._extra_body(cfg)
        if extra:
            kwargs["extra_body"] = extra

        client = self._conn.api(
            base_url=cfg.base_url, api_key_env=cfg.api_key_env,
            timeout=cfg.timeout_sec, what="vision",
        )
        # The deadline per request, as the chat path does: the client's own timeout
        # is whichever config built it, and a /reload that changes it must apply.
        kwargs["timeout"] = cfg.timeout_sec
        try:
            res = await asyncio.wait_for(
                client.chat.completions.create(**kwargs), timeout=cfg.timeout_sec
            )
        except (TimeoutError, openai.APITimeoutError):
            # The estimate: an image resolves to at most a few thousand prompt
            # tokens on these backends, and a deliberating describe measures under
            # a thousand out.
            est_in = 2500 + len(prompt)
            await _book_timeout(
                kind=Kind.VISION, model=cfg.model,
                cny=self.rate_for(cfg.model).tokens(0, est_in, 1000),
                group_id=group_id, in_miss=est_in, out=1000,
            )
            raise
        usage = res.usage.model_dump() if res.usage else {}
        in_miss = usage.get("prompt_tokens", 0) or 0
        out = usage.get("completion_tokens", 0) or 0
        # Billed under whatever answered, like the chat path: a vendor retiring an id
        # answers it with the successor, and pricing the call from the retired entry
        # bills a rate nobody charged.
        billed = getattr(res, "model", "") or cfg.model
        await BUDGET.record(
            kind=Kind.VISION, model=billed,
            cny=self.rate_for(billed).tokens(0, in_miss, out),
            group_id=group_id, in_miss=in_miss, out=out,
        )
        # No choice at all is the contract's "nothing to say", not a broken reply:
        # a filtered or empty answer arrives that way on some backends.
        if not res.choices:
            return ""
        return " ".join((res.choices[0].message.content or "").split())


# -- ASR --------------------------------------------------------------------


class OpenAICompatAsr(AsrModel):
    """Transcription via a chat completion carrying inline audio."""

    name = "openai_compat"

    def __init__(self) -> None:
        self._conn = OpenAIClient()

    async def aclose(self) -> None:
        await self._conn.aclose()

    def rate_for(self, model: str) -> Rate:
        return UNKNOWN_SECOND_RATE

    def _extra_body(self) -> dict[str, Any]:
        return {}

    def _audio_uri(self, b64: str, fmt: str) -> str:
        return f"data:audio/{fmt};base64,{b64}"

    async def transcribe(
        self, data: bytes, *, cfg: AsrCfg, fmt: str = "wav",
        seconds: float | None = None, group_id: str | None = None,
    ) -> str:
        b64 = base64.b64encode(data).decode()
        kwargs: dict[str, Any] = {
            "model": cfg.model,
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": ""}]},
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "input_audio",
                            "input_audio": {"data": self._audio_uri(b64, fmt), "format": fmt},
                        }
                    ],
                },
            ],
            "modalities": ["text"],
        }
        extra = self._extra_body()
        if extra:
            kwargs["extra_body"] = extra

        client = self._conn.api(
            base_url=cfg.base_url, api_key_env=cfg.api_key_env,
            timeout=cfg.timeout_sec, what="ASR",
        )
        # Billed per second of audio; when the duration is unknown assume 32 kbps.
        secs = seconds if seconds else len(data) / 4000.0
        kwargs["timeout"] = cfg.timeout_sec
        try:
            res = await asyncio.wait_for(
                client.chat.completions.create(**kwargs), timeout=cfg.timeout_sec
            )
        except (TimeoutError, openai.APITimeoutError):
            # The clip is billed by its full duration whether or not the answer
            # arrived in time, and the duration is known - so this booking is exact
            # rather than an estimate.
            await _book_timeout(
                kind=Kind.ASR, model=cfg.model,
                cny=self.rate_for(cfg.model).units(secs), group_id=group_id,
            )
            raise
        # Under whatever answered, as on the other two paths. The timeout above has no
        # response to read, so it books the id it asked for and can do no better.
        billed = getattr(res, "model", "") or cfg.model
        await BUDGET.record(
            kind=Kind.ASR, model=billed,
            cny=self.rate_for(billed).units(secs), group_id=group_id,
        )
        if not res.choices:
            return ""          # nothing was said, by the contract - not an error
        content = res.choices[0].message.content
        if isinstance(content, list):  # some backends return the multimodal array form
            content = "".join(p.get("text", "") for p in content if isinstance(p, dict))
        return " ".join((content or "").split())
