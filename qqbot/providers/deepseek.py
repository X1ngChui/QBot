"""DeepSeek Responses dialect, attachment storage and time-based pricing."""

from __future__ import annotations

import logging
import hashlib
from dataclasses import replace
from datetime import datetime, timedelta
from zoneinfo import ZoneInfo

import httpx

from qqbot.services.budget import Budget
from qqbot.configuration import TextCfg
from qqbot.configuration import VisionCfg
from qqbot.util import require_key
from qqbot.providers.base import Rate
from qqbot.providers.base import RetryPolicy
from qqbot.providers.base import with_retry
from qqbot.providers.contracts import ReasoningEffort
from qqbot.providers.contracts import Role
from qqbot.providers.contracts import StoredImage
from qqbot.providers.openai_responses import ResponsesCodec
from qqbot.providers.openai_responses import ResponsesTextModel
from qqbot.providers.openai_responses import ResponsesVisionModel

log = logging.getLogger("qqbot.deepseek")

_PRICES = {
    "deepseek-v4-flash": Rate(
        "Mtoken",
        in_hit=0.05,
        in_miss=1.5,
        out=4.5,
        source="deepseek repricing eff. 2026-08-17",
    ),
    "deepseek-v4-flash-vision-exp": Rate(
        "Mtoken",
        in_hit=0.05,
        in_miss=1.5,
        out=4.5,
        source="deepseek vision launch note: priced as V4-Flash",
    ),
    "deepseek-v4-pro": Rate(
        "Mtoken",
        in_hit=0.15,
        in_miss=4.5,
        out=13.5,
        source="deepseek pricing page, verified 2026-09-10",
    ),
    "deepseek-flash": Rate(
        "Mtoken",
        in_hit=0.02,
        in_miss=1.0,
        out=4.0,
        source="deepseek pricing page, verified 2026-09-10",
    ),
}
_UNKNOWN = Rate("Mtoken", in_hit=4.5, in_miss=4.5, out=13.5, source="pessimistic guess")
PEAK_MULTIPLIER = 2.0
_PEAK_HOURS = frozenset(range(9, 12)) | frozenset(range(14, 18))
_BILLING_TZ = ZoneInfo("Asia/Shanghai")
_WARNED_UNKNOWN: set[str] = set()


def _at_peak(now: datetime | None = None) -> bool:
    now = (now or datetime.now(_BILLING_TZ)).astimezone(_BILLING_TZ)
    return now.weekday() < 5 and now.hour in _PEAK_HOURS


def _rate_at(model: str, now: datetime) -> Rate:
    now = now.astimezone(_BILLING_TZ)
    rate = _PRICES.get(model)
    if rate is None:
        if model not in _WARNED_UNKNOWN:
            _WARNED_UNKNOWN.add(model)
            log.warning("no price entry for model %r: billing at the priciest tier", model)
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


def rate_for(model: str) -> Rate:
    return _rate_at(model, datetime.now(_BILLING_TZ))


class DeepSeekResponsesCodec(ResponsesCodec):
    """The small set of DeepSeek differences from standard Responses."""

    name = "deepseek"

    def role(self, role: Role) -> str:
        return Role.USER.value if role is Role.DEVELOPER else role.value

    def request_extras(self, effort: ReasoningEffort) -> dict[str, object]:
        return {
            "reasoning": {
                "effort": {
                    ReasoningEffort.OFF: "none",
                    ReasoningEffort.LOW: "low",
                    ReasoningEffort.HIGH: "high",
                    ReasoningEffort.MAX: "max",
                }[effort]
            }
        }


class DeepSeekAttachmentStore:
    """Files API storage scoped to one configured DeepSeek text account."""

    FILE_TTL_SEC = 30 * 24 * 3600
    cache_max_age = timedelta(seconds=FILE_TTL_SEC) - timedelta(minutes=5)

    def __init__(self, cfg: TextCfg, retry: RetryPolicy) -> None:
        self._endpoint = cfg.endpoint.rstrip("/")
        self._credential_env = cfg.credential_env
        self._retry = retry
        self._client = httpx.AsyncClient(timeout=cfg.timeout_sec)

    @property
    def cache_namespace(self) -> str:
        key = require_key(self._credential_env, "text")
        identity = repr((self._endpoint, key)).encode()
        return hashlib.sha256(identity).hexdigest()[:32]

    async def store(self, data: bytes, media_type: str) -> StoredImage:
        key = require_key(self._credential_env, "text")
        extension = media_type.partition("/")[2] or "bin"

        async def post() -> httpx.Response:
            response = await self._client.post(
                f"{self._endpoint}/files",
                headers={"Authorization": f"Bearer {key}"},
                files={"file": (f"img.{extension}", data, media_type)},
                data={
                    "purpose": "user_data",
                    "expires_after[anchor]": "created_at",
                    "expires_after[seconds]": str(self.FILE_TTL_SEC),
                },
            )
            response.raise_for_status()
            return response

        response = await with_retry(post, what="file upload", policy=self._retry)
        handle = str((response.json() or {}).get("id") or "")
        if not handle:
            raise RuntimeError("DeepSeek file upload returned no id")
        return StoredImage("deepseek", handle)

    async def aclose(self) -> None:
        await self._client.aclose()


def deepseek_text(cfg: TextCfg, retry: RetryPolicy, budget: Budget) -> ResponsesTextModel:
    return ResponsesTextModel(
        cfg,
        retry,
        name="deepseek",
        codec=DeepSeekResponsesCodec(),
        rate_for=rate_for,
        budget=budget,
        attachments=DeepSeekAttachmentStore(cfg, retry),
    )


def deepseek_vision(cfg: VisionCfg, retry: RetryPolicy, budget: Budget) -> ResponsesVisionModel:
    return ResponsesVisionModel(
        cfg,
        retry,
        name="deepseek",
        codec=DeepSeekResponsesCodec(),
        rate_for=rate_for,
        budget=budget,
    )
