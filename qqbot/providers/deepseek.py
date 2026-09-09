"""DeepSeek text backend.

Four things differ from a plain OpenAI-compatible endpoint, and they all live here:

1. Prompt-cache accounting is reported as `prompt_cache_hit_tokens` /
   `prompt_cache_miss_tokens`. A hit is ~30x cheaper than a miss, which is what the
   prompt ordering discipline in section 6.2 exists to earn - so the split has to be
   read, not assumed.
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

from ..settings import VisionCfg
from ..util import read_api_key
from .base import Rate
from .openai_compat import OpenAICompatChat, OpenAICompatVision

log = logging.getLogger("qqbot.deepseek")


# CNY per 1M tokens, off-peak figures; peak doubles them - see _at_peak. Two eras,
# because the vendor repriced effective 2026-08-17 00:00 Beijing time. Off-peak stays
# half of peak in both schemes, so the table-plus-multiplier structure holds and only
# the numbers change. Figures re-verified against the official pricing page 2026-09-09.
_PRICES_LEGACY = {
    "deepseek-v4-flash": Rate("Mtoken", in_hit=0.02, in_miss=1.0, out=2.0, source="deepseek docs"),
    "deepseek-v4-pro": Rate("Mtoken", in_hit=0.025, in_miss=3.0, out=6.0, source="deepseek docs"),
}
_PRICES_20260817 = {
    "deepseek-v4-flash": Rate("Mtoken", in_hit=0.05, in_miss=1.5, out=4.5,
                              source="deepseek repricing eff. 2026-08-17"),
    # Image tokens are input tokens: a picture scales to at most 384 of them and then
    # bills like any other prompt content, per the launch note (2026-08-21). The model
    # postdates the repricing, so only this era lists it.
    "deepseek-v4-flash-vision-exp": Rate("Mtoken", in_hit=0.05, in_miss=1.5, out=4.5,
                                         source="deepseek vision launch note: priced as V4-Flash"),
    "deepseek-v4-pro": Rate("Mtoken", in_hit=0.15, in_miss=4.5, out=13.5,
                            source="deepseek repricing eff. 2026-08-17"),
    # V4.1-Flash, announced to bill exactly like V4-Flash. Both the timed beta
    # id and the expected release id are listed: an unpriced model falls to the
    # pessimistic tier, whose cache-hit rate is 30x pro's, and every reply it
    # serves is then overbilled into the daily cap.
    "deepseek-v4.1-flash-expires-on-0910": Rate(
        "Mtoken", in_hit=0.05, in_miss=1.5, out=4.5,
        source="deepseek beta notice: priced as V4-Flash"),
    "deepseek-v4.1-flash": Rate(
        "Mtoken", in_hit=0.05, in_miss=1.5, out=4.5,
        source="deepseek beta notice: priced as V4-Flash; confirm at release"),
}
# An unlisted model bills at the most expensive tier known, so a rename cannot quietly
# make spending look smaller than it is.
_UNKNOWN_LEGACY = Rate("Mtoken", in_hit=3.0, in_miss=3.0, out=6.0, source="pessimistic guess")
_UNKNOWN_20260817 = Rate("Mtoken", in_hit=4.5, in_miss=4.5, out=13.5, source="pessimistic guess")

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

#: When the 2026-08-17 price table takes over, midnight Beijing time. Encoded as a switch
#: rather than edited in place on the day: a deploy is not going to happen at midnight,
#: and either constant alone would misbill for however long the gap lasted.
_REPRICE_AT = datetime(2026, 8, 17, tzinfo=_BILLING_TZ)

#: Models already reported as unpriced, so the warning below fires once each.
_WARNED_UNKNOWN: set[str] = set()


def _rate_at(model: str, now: datetime) -> Rate:
    """The rate for one model at one moment: era table, then peak doubling.

    A pure function of the clock so the two boundaries that decide real money - the
    repricing date and the peak window - can be pinned by tests instead of trusted to
    comments.
    """
    now = now.astimezone(_BILLING_TZ)
    table, unknown = ((_PRICES_20260817, _UNKNOWN_20260817)
                      if now >= _REPRICE_AT
                      else (_PRICES_LEGACY, _UNKNOWN_LEGACY))
    rate = table.get(model)
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
        rate = unknown
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

    def _extra_body(self, *, cfg, effort: str) -> dict[str, Any]:
        """The resolved deliberation grade as this vendor's request fields: "off"
        disables thinking, low/high/max enable it at that reasoning_effort (the
        vendor's own grades; its default is high, medium/xhigh alias to high)."""
        if effort == "off":
            return {"thinking": {"type": "disabled"}}
        return {"thinking": {"type": "enabled"}, "reasoning_effort": effort}


class DeepSeekVision(OpenAICompatVision):
    """Vision over the same account and price table as the chat backend.

    Two things beyond the generic class. Rates come from the era-and-peak function
    above, so a describe bills like the chat call it technically is. And this backend
    keeps files: upload() stores an image once with the vendor and returns a file_id
    that any later chat message can reference - which is what lets the reply path show
    the model an original picture instead of a one-line description of it.
    """

    name = "deepseek"

    def __init__(self) -> None:
        super().__init__()
        self._files: httpx.AsyncClient | None = None

    def rate_for(self, model: str) -> Rate:
        return _rate_at(model, datetime.now(_BILLING_TZ))

    def _extra_body(self, cfg) -> dict:
        # The describing call's grade comes from vision config: "off" for the old
        # no-thinking behaviour, "low" for sanity-check thinking at a fraction of the
        # ~800 thought tokens a picture that the vendor default measured at.
        if cfg.reasoning_effort == "off":
            return {"thinking": {"type": "disabled"}}
        return {"thinking": {"type": "enabled"}, "reasoning_effort": cfg.reasoning_effort}

    #: Uploaded files expire at the vendor after this long, which is what keeps the
    #: account's 10k-file store from silting up with no cleanup job to run or forget.
    #: Comfortably past prompt.PROMPT_IMAGE_MAX_AGE, the age at which the reply prompt
    #: stops attaching a picture at all: expiring first would leave the prompt holding
    #: file ids the vendor has already dropped.
    FILE_TTL_SEC = 30 * 24 * 3600

    async def upload(
        self, data: bytes, *, cfg: VisionCfg, mime: str = "image/jpeg",
    ) -> str | None:
        key = read_api_key(cfg.api_key_env)
        if not key:
            raise RuntimeError(f"no vision API key: {cfg.api_key_env} resolved to nothing")
        if self._files is None:
            self._files = httpx.AsyncClient(timeout=cfg.timeout_sec)
        ext = (mime.split("/", 1) + ["bin"])[1]
        r = await self._files.post(
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
        fid = (r.json() or {}).get("id") or ""
        return fid or None

    async def aclose(self) -> None:
        await super().aclose()
        if self._files is not None:
            await self._files.aclose()
            self._files = None
