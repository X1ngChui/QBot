"""DeepSeek text backend.

Four things differ from a plain OpenAI-compatible endpoint, and they all live here:

1. Prompt-cache accounting is reported as `prompt_cache_hit_tokens` /
   `prompt_cache_miss_tokens`. A hit is ~30x cheaper than a miss, which is what the
   prompt's stable-first ordering exists to earn - so the split has to be read from
   the response, not assumed.
2. These models deliberate before answering, and the deliberation bills as output. It
   arrives as `reasoning_content` and is reported under
   `completion_tokens_details.reasoning_tokens`.
3. Deliberation can be switched off per request. Measured on a binary yes/no prompt:
   7 output tokens and 1.1s with it off, 66 and 2.0s with it on.
4. What a call costs depends on when it is made: peak hours bill at double. See below.
"""

from __future__ import annotations

import logging
from dataclasses import replace
from datetime import datetime
from typing import Any
from zoneinfo import ZoneInfo

import httpx

from ..settings import TextCfg
from ..util import require_key
from .base import Rate, retire, with_retry
from .openai_compat import OpenAICompatChat, OpenAICompatVision

log = logging.getLogger("qqbot.deepseek")


# CNY per 1M tokens, off-peak figures; peak doubles them - see _at_peak. The
# vendor's pricing as of 2026-08-17; figures re-verified against the official pricing
# page 2026-09-09. When the vendor reprices, the table changes with it: the ledger
# books at the rate in force when a call is made, so no older table is kept.
_PRICES = {
    "deepseek-v4-flash": Rate("Mtoken", in_hit=0.05, in_miss=1.5, out=4.5,
                              source="deepseek repricing eff. 2026-08-17"),
    # Image tokens are input tokens: a picture scales to at most 384 of them and then
    # bills like any other prompt content, per the launch note (2026-08-21).
    "deepseek-v4-flash-vision-exp": Rate("Mtoken", in_hit=0.05, in_miss=1.5, out=4.5,
                                         source="deepseek vision launch note: priced as V4-Flash"),
    "deepseek-v4-pro": Rate("Mtoken", in_hit=0.15, in_miss=4.5, out=13.5,
                            source="deepseek pricing page, verified 2026-09-10"),
    # The current flash tier (V4.1-Flash). The vendor routes calls naming the two
    # flash ids above here as well, and from 2026-09-14 12:00 Beijing those naming
    # deepseek-v4-pro, until a V4.1 Pro ships. A routed call bills at these rates
    # whatever id asked, because the ledger books the model the response reports,
    # not the one the request named - so nothing in config has to change.
    "deepseek-flash": Rate("Mtoken", in_hit=0.02, in_miss=1.0, out=4.0,
                           source="deepseek pricing page, verified 2026-09-10"),
}
# An unlisted model bills at the most expensive tier known, so a rename cannot quietly
# make spending look smaller than it is.
_UNKNOWN = Rate("Mtoken", in_hit=4.5, in_miss=4.5, out=13.5, source="pessimistic guess")

#: Time-of-day billing, announced 2026-06-29 and in force since mid-July. Every component
#: doubles during peak - a cache hit as much as an output token.
#:
#: The tables above hold the off-peak figures, so this multiplier is load-bearing: a
#: group chats most in exactly the peak hours, and the ledger is what the daily cap is
#: measured against, so booking peak calls at off-peak rates would compare the ceiling
#: with a number that is too small precisely when spending is fastest.
PEAK_MULTIPLIER = 2.0
#: Weekdays 09:00-12:00 and 14:00-18:00. Lunch is off-peak, and so is everything after
#: six.
_PEAK_HOURS = frozenset(range(9, 12)) | frozenset(range(14, 18))
#: The vendor bills in Beijing time whatever timezone this deployment reports in, so this
#: is fixed rather than read from config. Using the configured zone would price correctly
#: only by the coincidence of it being the same one.
_BILLING_TZ = ZoneInfo("Asia/Shanghai")

#: Models already reported as unpriced, so the warning below fires once each.
_WARNED_UNKNOWN: set[str] = set()


def _rate_at(model: str, now: datetime) -> Rate:
    """The rate for one model at one moment: the table, then peak doubling.

    A pure function of the clock so the boundary that decides real money - the
    peak window - can be pinned by tests instead of trusted to comments.
    """
    now = now.astimezone(_BILLING_TZ)
    rate = _PRICES.get(model)
    if rate is None:
        # Loudly, once per model. The pessimistic tier keeps the daily cap safe
        # by overbilling, which also means an unpriced model burns its reply
        # scope several times faster than it should - fewer tool rounds per
        # reply, and a daily cap that trips early.
        if model not in _WARNED_UNKNOWN:
            _WARNED_UNKNOWN.add(model)
            log.warning("no price entry for model %r: billing at the priciest "
                        "tier, so this model overspends its per-reply scope and "
                        "the daily cap. Add it to the table.", model)
        rate = _UNKNOWN
    if not _at_peak(now):
        return rate
    return replace(
        rate,
        in_hit=rate.in_hit * PEAK_MULTIPLIER,
        in_miss=rate.in_miss * PEAK_MULTIPLIER,
        out=rate.out * PEAK_MULTIPLIER,
        source=rate.source + ", peak hours",
    )


def _at_peak(now: datetime | None = None) -> bool:
    """Whether a call made now bills at the peak rate. `now` must be timezone-aware.

    Converted into the billing zone rather than read as it arrives: the hour that decides
    this is the hour in Beijing, and taking `.hour` off whatever was passed in would put
    the boundary wherever the caller's clock happens to sit.

    Public holidays are off-peak too, and are not accounted for here: the calendar is set
    annually and includes swapped working weekends, so there is nothing to compute it
    from. Billing a holiday as peak overestimates, which is the direction every other
    guess in this file leans - it trips the budget gate early rather than spending money
    the gate never sees.
    """
    now = (now or datetime.now(_BILLING_TZ)).astimezone(_BILLING_TZ)
    return now.weekday() < 5 and now.hour in _PEAK_HOURS


class DeepSeekChat(OpenAICompatChat):
    name = "deepseek"
    keeps_files = True

    def __init__(self) -> None:
        super().__init__()
        #: Its own client: the Files endpoint is plain multipart, not the chat
        #: protocol the SDK speaks. Rebuilt when the timeout it was built for
        #: changes, so a reload reaches it like every other client here.
        self._files: httpx.AsyncClient | None = None
        self._files_timeout = 0.0

    async def aclose(self) -> None:
        await super().aclose()
        if self._files is not None:
            await self._files.aclose()
            self._files = None
            self._files_timeout = 0.0

    def rate_for(self, model: str) -> Rate:
        """What this model costs to call right now.

        Time-dependent twice over - the vendor prices by hour of day and repriced whole
        tables on 2026-08-17 - so it is only ever asked at the moment a call is being
        booked, which is the moment the answer is about.
        """
        return _rate_at(model, datetime.now(_BILLING_TZ))

    def _usage_tokens(self, usage: dict) -> tuple[int, int, int, int]:
        total_in = usage.get("prompt_tokens", 0) or 0
        hit = usage.get("prompt_cache_hit_tokens")
        miss = usage.get("prompt_cache_miss_tokens")
        if hit is None and miss is None:
            in_hit, in_miss = 0, total_in
        else:
            # Tolerant in both directions: whichever half is missing is derived from
            # the total. Derived one-sidedly, a usage carrying only the miss field
            # billed the hit share at nothing - under-billing, the wrong direction
            # to guess in.
            in_hit = hit if hit is not None else max(0, total_in - (miss or 0))
            in_miss = miss if miss is not None else max(0, total_in - in_hit)
        details = usage.get("completion_tokens_details") or {}
        return (
            in_hit,
            in_miss,
            usage.get("completion_tokens", 0) or 0,
            details.get("reasoning_tokens") or 0,
        )

    def _image_part(self, image_id: str) -> dict[str, Any]:
        """Flat, not nested. The vendor rejects OpenAI's {"file": {"file_id": ...}}
        with "file must have a file_id or file_data"; it takes an image_url data URI
        too, but that re-sends the bytes on every round of every reply, and a picture
        in the window is re-sent on all of them."""
        return {"type": "file", "file_id": image_id}

    #: Uploaded files expire at the vendor after this long, which is what keeps the
    #: account's file store from silting up with no cleanup job to run or forget.
    #: It must stay comfortably above llm.vision.file_max_age_days, the age past
    #: which a stored file id is re-uploaded rather than trusted: expiring first
    #: would let open_images hand the model an id the vendor has already dropped.
    FILE_TTL_SEC = 30 * 24 * 3600

    async def upload(
        self, data: bytes, *, cfg: TextCfg, mime: str = "image/jpeg",
    ) -> str | None:
        key = require_key(cfg.api_key_env, "text")
        if self._files is None or self._files_timeout != cfg.timeout_sec:
            if self._files is not None:
                retire(self._files.aclose())
            self._files = httpx.AsyncClient(timeout=cfg.timeout_sec)
            self._files_timeout = cfg.timeout_sec
        # The vendor takes the format from the filename, so give it the one the
        # mime type names.
        ext = mime.partition("/")[2] or "bin"
        client = self._files

        async def post() -> httpx.Response:
            r = await client.post(
                f"{cfg.base_url.rstrip('/')}/files",
                headers={"Authorization": f"Bearer {key}"},
                files={"file": (f"img.{ext}", data, mime)},
                data={
                    "purpose": "user_data",
                    "expires_after[anchor]": "created_at",
                    "expires_after[seconds]": str(self.FILE_TTL_SEC),
                },
            )
            r.raise_for_status()
            return r

        r = await with_retry(post, what="file upload")
        fid = (r.json() or {}).get("id") or ""
        return fid or None

    def _extra_body(self, *, cfg, effort: str) -> dict[str, Any]:
        """The resolved deliberation grade as this vendor's request fields: "off"
        disables thinking, low/high/max enable it at that reasoning_effort (the
        vendor's own grades; its default is high, medium/xhigh alias to high)."""
        if effort == "off":
            return {"thinking": {"type": "disabled"}}
        return {"thinking": {"type": "enabled"}, "reasoning_effort": effort}


class DeepSeekVision(OpenAICompatVision):
    """Vision over the same account and price table as the chat backend.

    One thing beyond the generic class: rates come from the era-and-peak function
    above, so a describe bills like the chat call it technically is. Keeping files
    belongs to the chat backend, because the model that reads a file block is the one
    that has to resolve its id.
    """

    name = "deepseek"

    def rate_for(self, model: str) -> Rate:
        return _rate_at(model, datetime.now(_BILLING_TZ))

    def _extra_body(self, cfg) -> dict:
        # The describing call's grade comes from vision config: "off" for the old
        # no-thinking behaviour, "low" for sanity-check thinking at a fraction of the
        # ~800 thought tokens a picture that the vendor default measured at.
        if cfg.reasoning_effort == "off":
            return {"thinking": {"type": "disabled"}}
        return {"thinking": {"type": "enabled"}, "reasoning_effort": cfg.reasoning_effort}

