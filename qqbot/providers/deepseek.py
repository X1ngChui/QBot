"""DeepSeek Responses dialect, attachment storage and time-based pricing."""

from __future__ import annotations

import asyncio
import logging
from dataclasses import replace
from datetime import datetime
from zoneinfo import ZoneInfo

import httpx

from ..settings import TextCfg, VisionCfg
from ..util import require_key
from .base import Rate, with_retry
from .contracts import ReasoningEffort, Role, StoredImage
from .openai_responses import (
    ResponsesCodec,
    ResponsesTextModel,
    ResponsesVisionModel,
)

log = logging.getLogger("qqbot.deepseek")

_PRICES = {
    "deepseek-v4-flash": Rate(
        "Mtoken", in_hit=0.05, in_miss=1.5, out=4.5,
        source="deepseek repricing eff. 2026-08-17",
    ),
    "deepseek-v4-flash-vision-exp": Rate(
        "Mtoken", in_hit=0.05, in_miss=1.5, out=4.5,
        source="deepseek vision launch note: priced as V4-Flash",
    ),
    "deepseek-v4-pro": Rate(
        "Mtoken", in_hit=0.15, in_miss=4.5, out=13.5,
        source="deepseek pricing page, verified 2026-09-10",
    ),
    "deepseek-flash": Rate(
        "Mtoken", in_hit=0.02, in_miss=1.0, out=4.0,
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

    def __init__(self, cfg: TextCfg) -> None:
        self._endpoint = cfg.endpoint.rstrip("/")
        self._credential_env = cfg.credential_env
        self._timeout = cfg.timeout_sec
        self._clients: dict[float, httpx.AsyncClient] = {}

    async def store(self, data: bytes, media_type: str) -> StoredImage:
        key = require_key(self._credential_env, "text")
        client = self._clients.get(self._timeout)
        if client is None:
            client = httpx.AsyncClient(timeout=self._timeout)
            self._clients[self._timeout] = client
        extension = media_type.partition("/")[2] or "bin"

        async def post() -> httpx.Response:
            response = await client.post(
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

        response = await with_retry(post, what="file upload")
        handle = str((response.json() or {}).get("id") or "")
        if not handle:
            raise RuntimeError("DeepSeek file upload returned no id")
        return StoredImage("deepseek", handle)

    async def aclose(self) -> None:
        clients, self._clients = tuple(self._clients.values()), {}
        await asyncio.gather(*(client.aclose() for client in clients))


def deepseek_text(cfg: TextCfg) -> ResponsesTextModel:
    return ResponsesTextModel(
        cfg,
        name="deepseek",
        codec=DeepSeekResponsesCodec(),
        rate_for=rate_for,
        attachments=DeepSeekAttachmentStore(cfg),
    )


def deepseek_vision(cfg: VisionCfg) -> ResponsesVisionModel:
    return ResponsesVisionModel(
        cfg,
        name="deepseek",
        codec=DeepSeekResponsesCodec(),
        rate_for=rate_for,
    )
